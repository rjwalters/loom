//! Loom workspace initialization module
//!
//! This module provides functionality for initializing Loom workspaces
//! without requiring the Tauri application. It can be used from:
//! - CLI mode (loom-daemon init)
//! - Tauri IPC commands (shared code)
//!
//! The initialization process:
//! 1. Validates the target is a git repository
//! 2. Detects self-installation (Loom source repo) and runs validation-only mode
//! 3. Copies `.loom/` configuration from `defaults/` (merge mode preserves custom files)
//! 4. Sets up repository scaffolding (CLAUDE.md, .claude/, .codex/)
//! 5. Updates .gitignore with Loom ephemeral patterns
//! 6. Reports which files were preserved vs added
//!
//! # Module Structure
//!
//! - [`git`]: Git detection, validation, and path resolution
//! - [`file_ops`]: File copy/merge/clean operations with reporting
//! - [`templates`]: Template variable substitution
//! - [`scaffolding`]: Repository scaffolding setup (CLAUDE.md, .claude/, etc.)
//! - [`post_init`]: Post-initialization operations (manifest, gitignore)

mod file_ops;
mod git;
mod post_init;
mod scaffolding;
mod templates;

use std::fs;
use std::path::Path;

use file_ops::{clean_managed_dir, copy_dir_with_report, verify_copied_files, TemplateContext};
use post_init::{generate_manifest, update_gitignore};
use scaffolding::setup_repository_scaffolding;

// Re-export public types and functions
pub use git::is_loom_source_repo;

// Import the rest for internal use
use git::{resolve_defaults_path, validate_git_repository, validate_loom_source_repo};

/// Report of files affected during initialization
///
/// This struct tracks which files were added from defaults vs preserved
/// from the existing installation, enabling users to identify custom files
/// and deprecated files that may need cleanup.
#[derive(Debug, Default)]
pub struct InitReport {
    /// Files that were added from defaults (didn't exist before)
    pub added: Vec<String>,
    /// Files that were preserved (existed before, not overwritten)
    pub preserved: Vec<String>,
    /// Files that were updated (existed before, overwritten on reinstall)
    pub updated: Vec<String>,
    /// Files that were removed (existed in destination but not in source, cleaned on reinstall)
    pub removed: Vec<String>,
    /// Files that failed post-copy verification (destination doesn't match source)
    pub verification_failures: Vec<String>,
    /// Whether this was a self-installation (Loom source repo)
    pub is_self_install: bool,
    /// Validation results for self-installation mode
    pub validation: Option<ValidationReport>,
}

/// Validation report for self-installation mode
#[derive(Debug, Default)]
pub struct ValidationReport {
    /// Role definitions found
    pub roles_found: Vec<String>,
    /// Scripts found
    pub scripts_found: Vec<String>,
    /// Slash commands found
    pub commands_found: Vec<String>,
    /// Whether CLAUDE.md exists
    pub has_claude_md: bool,
    /// Whether .github/labels.yml exists
    pub has_labels_yml: bool,
    /// Issues found during validation
    pub issues: Vec<String>,
}

/// Initialize a Loom workspace in the target directory
///
/// # Arguments
///
/// * `workspace_path` - Path to the workspace directory (must be a git repository)
/// * `defaults_path` - Path to the defaults directory (usually "defaults" or bundled resource)
/// * `force` - If true, overwrite existing files (otherwise merge mode preserves custom files)
///
/// # Returns
///
/// * `Ok(InitReport)` - Workspace successfully initialized with report of changes
/// * `Err(String)` - Initialization failed with error message
///
/// # Behavior
///
/// - **Fresh install** (no .loom directory): Copies all files from defaults
/// - **Reinstall with force=false** (merge mode): Adds new files, preserves ALL existing files
/// - **Reinstall with force=true** (force-merge mode): Updates default files, preserves custom files
///
/// Both reinstall modes preserve custom project roles/commands (files not in defaults).
/// Force mode is useful when you want to update Loom's built-in roles to the latest version.
///
/// # Errors
///
/// This function will return an error if:
/// - The workspace path doesn't exist or isn't a directory
/// - The workspace isn't a git repository (no .git directory)
/// - File operations fail (insufficient permissions, disk full, etc.)
pub fn initialize_workspace(
    workspace_path: &str,
    defaults_path: &str,
    force: bool,
) -> Result<InitReport, String> {
    let workspace = Path::new(workspace_path);
    let loom_path = workspace.join(".loom");
    let mut report = InitReport::default();

    // Validate workspace is a git repository
    validate_git_repository(workspace_path)?;

    // Check for self-installation (Loom source repo)
    if is_loom_source_repo(workspace) {
        report.is_self_install = true;
        report.validation = Some(validate_loom_source_repo(workspace));
        update_gitignore(workspace)?;
        return Ok(report);
    }

    // Resolve defaults path (development mode or bundled resource)
    let defaults = resolve_defaults_path(defaults_path)?;
    let is_reinstall = loom_path.exists();
    let _ = (is_reinstall, force); // These affect behavior in called functions

    // Create .loom directory if it doesn't exist
    fs::create_dir_all(&loom_path).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy config and README files
    copy_single_file(&defaults, &loom_path, "config.json", ".loom/config.json", &mut report)?;
    copy_single_file(&defaults, &loom_path, ".loom-README.md", ".loom/README.md", &mut report)?;

    // Sync managed directories (clean stale files on reinstall, then copy fresh)
    sync_managed_dir(&defaults, &loom_path, "roles", is_reinstall, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "scripts", is_reinstall, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "hooks", is_reinstall, &mut report)?;
    make_hooks_executable(&loom_path.join("hooks"));

    // Update .gitignore and setup scaffolding
    update_gitignore(workspace)?;
    setup_repository_scaffolding(workspace, &defaults, force, &mut report)?;

    // Verify all copied files match their sources
    verify_all_copied_files(workspace, &defaults, &loom_path, &mut report);

    // Generate installation manifest (.loom/manifest.json)
    generate_manifest(workspace);

    Ok(report)
}

/// Copy a single file from defaults to the loom directory, tracking in report.
fn copy_single_file(
    defaults: &Path,
    loom_path: &Path,
    src_name: &str,
    report_name: &str,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(src_name);
    // The destination may differ from the source name (e.g., ".loom-README.md" → "README.md")
    let dst_name = report_name.strip_prefix(".loom/").unwrap_or(src_name);
    let dst = loom_path.join(dst_name);
    if src.exists() {
        let existed = dst.exists();
        fs::copy(&src, &dst).map_err(|e| format!("Failed to copy {src_name}: {e}"))?;
        if existed {
            report.updated.push(report_name.to_string());
        } else {
            report.added.push(report_name.to_string());
        }
    }
    Ok(())
}

/// Sync a managed directory: clean stale files on reinstall, then copy fresh from defaults.
fn sync_managed_dir(
    defaults: &Path,
    loom_path: &Path,
    dir_name: &str,
    is_reinstall: bool,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(dir_name);
    let dst = loom_path.join(dir_name);
    let report_prefix = format!(".loom/{dir_name}");
    if src.exists() {
        if is_reinstall {
            clean_managed_dir(&dst, &report_prefix, report)
                .map_err(|e| format!("Failed to clean {dir_name} directory: {e}"))?;
        }
        copy_dir_with_report(&src, &dst, &report_prefix, report)
            .map_err(|e| format!("Failed to copy {dir_name} directory: {e}"))?;
    }
    Ok(())
}

/// Ensure all `.sh` files in the hooks directory are executable.
fn make_hooks_executable(hooks_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(hooks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sh") {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = std::fs::metadata(&path) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(perms.mode() | 0o111);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
            }
        }
    }
}

/// Verify all copied files and scaffolding directories match their sources.
fn verify_all_copied_files(
    workspace: &Path,
    defaults: &Path,
    loom_path: &Path,
    report: &mut InitReport,
) {
    // Verify .loom managed directories (no template substitution needed)
    for dir_name in &["roles", "scripts", "hooks"] {
        let src = defaults.join(dir_name);
        let dst = loom_path.join(dir_name);
        let prefix = format!(".loom/{dir_name}");
        verify_copied_files(&src, &dst, &prefix, report, None);
    }

    // Verify scaffolding directories with template context for variable substitution
    let repo_info = git::extract_repo_info(workspace);
    let template_ctx = TemplateContext {
        repo_owner: repo_info.as_ref().map(|(o, _)| o.clone()),
        repo_name: repo_info.map(|(_, n)| n),
        loom_metadata: templates::LoomMetadata::from_env(),
    };
    let ctx = Some(&template_ctx);

    for dir_name in &[".claude", ".codex", ".github"] {
        let src = defaults.join(dir_name);
        let dst = workspace.join(dir_name);
        verify_copied_files(&src, &dst, dir_name, report, ctx);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_is_loom_source_repo_marker_file() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Initially not a Loom source repo
        assert!(!is_loom_source_repo(workspace));

        // Create marker file
        fs::write(workspace.join(".loom-source"), "").unwrap();

        // Now it should be detected as Loom source repo
        assert!(is_loom_source_repo(workspace));
    }

    #[test]
    fn test_is_loom_source_repo_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Initially not a Loom source repo
        assert!(!is_loom_source_repo(workspace));

        // Create partial structure (not enough)
        fs::create_dir(workspace.join("src-tauri")).unwrap();
        assert!(!is_loom_source_repo(workspace));

        // Create more structure
        fs::create_dir(workspace.join("loom-daemon")).unwrap();
        assert!(!is_loom_source_repo(workspace));

        // Create defaults directory
        fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
        fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

        // Now it should be detected as Loom source repo
        assert!(is_loom_source_repo(workspace));
    }

    #[test]
    fn test_self_install_returns_validation_report() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Create git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create Loom source structure
        fs::create_dir(workspace.join("src-tauri")).unwrap();
        fs::create_dir(workspace.join("loom-daemon")).unwrap();
        fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
        fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

        // Create minimal .loom structure
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
        fs::write(workspace.join(".loom").join("roles").join("builder.md"), "").unwrap();
        fs::write(workspace.join(".loom").join("scripts").join("worktree.sh"), "").unwrap();

        // Create .claude/commands/
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("builder.md"),
            "",
        )
        .unwrap();

        // Create docs
        fs::write(workspace.join("CLAUDE.md"), "").unwrap();

        // Create labels.yml
        fs::create_dir_all(workspace.join(".github")).unwrap();
        fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

        // Run initialization
        let result = initialize_workspace(
            workspace.to_str().unwrap(),
            "nonexistent-defaults", // Should not be used for self-install
            false,
        );

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify self-install detection
        assert!(report.is_self_install);
        assert!(report.validation.is_some());

        let validation = report.validation.unwrap();
        assert!(validation.roles_found.contains(&"builder".to_string()));
        assert!(validation.scripts_found.contains(&"worktree".to_string()));
        assert!(validation.commands_found.contains(&"builder".to_string()));
        assert!(validation.has_claude_md);
        assert!(validation.has_labels_yml);
    }

    #[test]
    fn test_roles_cleaned_and_updated_on_reinstall() {
        // On reinstall, managed directories (roles/, scripts/) are cleaned first
        // to remove stale files, then fresh defaults are copied in.
        // Custom files that aren't in defaults are removed (not preserved).
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with roles
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "new builder content v2").unwrap();
        fs::write(defaults.join("roles").join("judge.md"), "new judge content").unwrap();

        // Create existing .loom directory (simulates previous install)
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::write(
            workspace.join(".loom").join("roles").join("builder.md"),
            "old builder content v1",
        )
        .unwrap();
        fs::write(workspace.join(".loom").join("roles").join("stale-role.md"), "stale role")
            .unwrap();

        // Run initialization WITHOUT force flag (simulates normal reinstall)
        let result = initialize_workspace(
            workspace.to_str().unwrap(),
            defaults.to_str().unwrap(),
            false, // No force flag
        );

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify: builder.md has new content from defaults
        let builder =
            fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
        assert_eq!(builder, "new builder content v2");

        // Verify: judge.md was ADDED (new default role)
        let judge =
            fs::read_to_string(workspace.join(".loom").join("roles").join("judge.md")).unwrap();
        assert_eq!(judge, "new judge content");

        // Verify: stale-role.md was REMOVED (not in defaults)
        assert!(
            !workspace
                .join(".loom")
                .join("roles")
                .join("stale-role.md")
                .exists(),
            "Stale role file should have been removed on reinstall"
        );

        // Verify report reflects the removal
        assert!(
            report
                .removed
                .contains(&".loom/roles/stale-role.md".to_string()),
            "Report should list stale-role.md as removed, got: {:?}",
            report.removed
        );

        // Both files from defaults should be reported as added (directory was cleaned first)
        assert!(report.added.contains(&".loom/roles/builder.md".to_string()));
        assert!(report.added.contains(&".loom/roles/judge.md".to_string()));
    }

    #[test]
    fn test_reinstall_removes_stale_files() {
        // Verifies that files in destination but not in source are removed on reinstall
        // This is the core behavior change for issue #1798
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with scripts (simulates Python port: old shell scripts removed)
        fs::create_dir_all(defaults.join("scripts")).unwrap();
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("scripts").join("worktree.sh"), "#!/bin/bash\n# kept").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder role").unwrap();

        // Create existing .loom with stale scripts (simulates pre-port state)
        fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::write(
            workspace.join(".loom").join("scripts").join("worktree.sh"),
            "#!/bin/bash\n# old",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("validate-phase.sh"),
            "#!/bin/bash\n# ported to python",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("agent-metrics.sh"),
            "#!/bin/bash\n# ported to python",
        )
        .unwrap();
        fs::write(workspace.join(".loom").join("roles").join("builder.md"), "old builder").unwrap();
        fs::write(workspace.join(".loom").join("roles").join("obsolete.md"), "removed role")
            .unwrap();

        // Run reinstall
        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Stale scripts should be removed
        assert!(
            !workspace
                .join(".loom")
                .join("scripts")
                .join("validate-phase.sh")
                .exists(),
            "validate-phase.sh should have been removed"
        );
        assert!(
            !workspace
                .join(".loom")
                .join("scripts")
                .join("agent-metrics.sh")
                .exists(),
            "agent-metrics.sh should have been removed"
        );

        // Stale role should be removed
        assert!(
            !workspace
                .join(".loom")
                .join("roles")
                .join("obsolete.md")
                .exists(),
            "obsolete.md should have been removed"
        );

        // Current files should exist with fresh content
        let worktree =
            fs::read_to_string(workspace.join(".loom").join("scripts").join("worktree.sh"))
                .unwrap();
        assert_eq!(worktree, "#!/bin/bash\n# kept");

        let builder =
            fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
        assert_eq!(builder, "builder role");

        // Report should track removals
        assert!(report
            .removed
            .contains(&".loom/scripts/validate-phase.sh".to_string()));
        assert!(report
            .removed
            .contains(&".loom/scripts/agent-metrics.sh".to_string()));
        assert!(report
            .removed
            .contains(&".loom/roles/obsolete.md".to_string()));
    }

    #[test]
    fn test_init_report_includes_verification() {
        // Full initialization should include verification with no failures
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("scripts").join("test.sh"), "#!/bin/bash").unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Fresh install should have zero verification failures
        assert!(
            report.verification_failures.is_empty(),
            "Expected no verification failures, got: {:?}",
            report.verification_failures
        );
    }

    #[test]
    fn test_hooks_installed_on_fresh_install() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with hooks
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("hooks")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("hooks").join("guard-destructive.sh"), "#!/bin/bash\n# guard hook")
            .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Hook should be installed
        let hook_path = workspace
            .join(".loom")
            .join("hooks")
            .join("guard-destructive.sh");
        assert!(hook_path.exists(), "Hook file should be installed");
        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, "#!/bin/bash\n# guard hook");

        // Report should list the hook as added
        assert!(report
            .added
            .contains(&".loom/hooks/guard-destructive.sh".to_string()));
    }

    #[test]
    fn test_hooks_preserved_on_reinstall() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with hooks
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("hooks")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("hooks").join("guard-destructive.sh"),
            "#!/bin/bash\n# updated guard hook v2",
        )
        .unwrap();

        // Simulate existing installation with old hook
        fs::create_dir_all(workspace.join(".loom").join("hooks")).unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("hooks")
                .join("guard-destructive.sh"),
            "#!/bin/bash\n# old guard hook v1",
        )
        .unwrap();

        // Run reinstall
        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());

        // Hook should have new content (clean-then-copy)
        let hook_path = workspace
            .join(".loom")
            .join("hooks")
            .join("guard-destructive.sh");
        assert!(hook_path.exists(), "Hook file should exist after reinstall");
        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, "#!/bin/bash\n# updated guard hook v2");
    }
}
