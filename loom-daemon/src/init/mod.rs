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

use std::collections::HashSet;
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
    make_shell_scripts_executable(&loom_path.join("hooks"));
    make_shell_scripts_executable(&loom_path.join("scripts"));

    // Update .gitignore and setup scaffolding
    update_gitignore(workspace)?;
    setup_repository_scaffolding(workspace, &defaults, force, &mut report)?;

    // Verify all copied files match their sources
    verify_all_copied_files(workspace, &defaults, &loom_path, &mut report);

    // Filter out verification failures for files that were intentionally preserved.
    // Preserved files (existing user customizations) are expected to differ from the
    // source defaults — flagging them as failures is misleading and the prior
    // "rerun with --force" remediation would clobber the user's intentional edits.
    filter_preserved_from_verification_failures(&mut report);

    // Generate installation manifest (.loom/manifest.json)
    generate_manifest(workspace);

    Ok(report)
}

/// Remove `verification_failures` entries whose path appears in `preserved`.
///
/// Verification failure entries are formatted as `"{rel_path} ({reason})"`. We
/// extract the leading path component and drop the entry if it matches a
/// preserved path. A preserved file is, by definition, expected to differ from
/// the source default (the user customized it), so it should not surface as a
/// "verification failure".
fn filter_preserved_from_verification_failures(report: &mut InitReport) {
    if report.preserved.is_empty() || report.verification_failures.is_empty() {
        return;
    }
    let preserved: HashSet<&str> = report.preserved.iter().map(String::as_str).collect();
    report.verification_failures.retain(|f| {
        let rel_path = f.split(" (").next().unwrap_or(f.as_str());
        !preserved.contains(rel_path)
    });
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
///
/// After copying, this function performs a fail-fast assertion that every file
/// (including those in subdirectories) present in `defaults/<dir_name>/` exists
/// in the destination. This guards against silent omissions like the regression
/// reported in issue #3220, where `scripts/lib/forge-helpers.sh` was missing
/// from installs even though `lib/loom-tools.sh` was verified by name.
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

        // Fail-fast: ensure every source file (including subdirectories) reached
        // the destination. This catches bugs in copy_dir_with_report and
        // unexpected filesystem failures (permission denied on a subdir, etc.)
        // before they propagate to a broken install.
        let missing = find_missing_files(&src, &dst);
        if !missing.is_empty() {
            return Err(format!(
                "Sync of {dir_name} directory completed but {} file(s) are missing from \
                 destination (likely a copy bug or filesystem error): {}",
                missing.len(),
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// Recursively find files present in `src` but missing from `dst`.
///
/// Returns relative paths (from `src`) for each missing file. Used as a
/// post-copy assertion to detect partial copies. Subdirectories are walked
/// recursively so that nested files (e.g., `scripts/lib/*.sh`) are checked.
fn find_missing_files(src: &Path, dst: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    collect_missing_files(src, dst, "", &mut missing);
    missing
}

fn collect_missing_files(src: &Path, dst: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();
        let rel_path = if prefix.is_empty() {
            file_name_str.to_string()
        } else {
            format!("{prefix}/{file_name_str}")
        };
        let src_child = entry.path();
        let dst_child = dst.join(&file_name);
        if file_type.is_dir() {
            collect_missing_files(&src_child, &dst_child, &rel_path, out);
        } else if !dst_child.exists() {
            out.push(rel_path);
        }
    }
}

/// Ensure all `.sh` files in a directory (and subdirectories) are executable.
///
/// This is applied to both hooks/ and scripts/ after copying from defaults.
/// While `fs::copy` preserves permissions on Unix, some git configurations
/// or filesystem operations may strip the execute bit. This ensures all
/// shell scripts remain executable regardless of how they were copied.
fn make_shell_scripts_executable(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                make_shell_scripts_executable(&path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("sh") {
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

        // Create .claude/commands/loom/
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
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

    #[test]
    fn test_scripts_lib_subdirectory_copied_on_fresh_install() {
        // Verifies that scripts/lib/ subdirectory (containing loom-tools.sh
        // and pipe-pane-cmd.sh) is correctly copied during initialization.
        // This is the specific scenario reported in issue #2392.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults mirroring real structure with scripts/lib/
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("scripts").join("agent-spawn.sh"),
            "#!/bin/bash\nsource \"$SCRIPT_DIR/lib/loom-tools.sh\"",
        )
        .unwrap();
        fs::write(
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
            "#!/bin/bash\n# shared helper library",
        )
        .unwrap();
        fs::write(
            defaults
                .join("scripts")
                .join("lib")
                .join("pipe-pane-cmd.sh"),
            "#!/bin/bash\n# pipe pane command",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify scripts/lib/ subdirectory exists
        let lib_dir = workspace.join(".loom").join("scripts").join("lib");
        assert!(lib_dir.exists(), "scripts/lib/ directory should exist");
        assert!(lib_dir.is_dir(), "scripts/lib/ should be a directory");

        // Verify both lib files were copied
        let loom_tools = lib_dir.join("loom-tools.sh");
        assert!(loom_tools.exists(), "lib/loom-tools.sh should exist");
        let content = fs::read_to_string(&loom_tools).unwrap();
        assert_eq!(content, "#!/bin/bash\n# shared helper library");

        let pipe_pane = lib_dir.join("pipe-pane-cmd.sh");
        assert!(pipe_pane.exists(), "lib/pipe-pane-cmd.sh should exist");

        // Verify the parent script was also copied
        let agent_spawn = workspace
            .join(".loom")
            .join("scripts")
            .join("agent-spawn.sh");
        assert!(agent_spawn.exists(), "agent-spawn.sh should exist");

        // Verify report includes subdirectory files
        assert!(
            report
                .added
                .contains(&".loom/scripts/lib/loom-tools.sh".to_string()),
            "Report should include lib/loom-tools.sh, got: {:?}",
            report.added
        );
        assert!(
            report
                .added
                .contains(&".loom/scripts/lib/pipe-pane-cmd.sh".to_string()),
            "Report should include lib/pipe-pane-cmd.sh, got: {:?}",
            report.added
        );

        // Verify no verification failures
        assert!(
            report.verification_failures.is_empty(),
            "Expected no verification failures, got: {:?}",
            report.verification_failures
        );
    }

    #[test]
    fn test_scripts_lib_subdirectory_restored_on_reinstall() {
        // On reinstall, scripts/lib/ should be cleaned and re-copied.
        // This tests the case where lib/ existed but with stale content.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with scripts/lib/
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
            "#!/bin/bash\n# v2 helper",
        )
        .unwrap();

        // Simulate existing installation with old lib/ content and a stale file
        fs::create_dir_all(workspace.join(".loom").join("scripts").join("lib")).unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("lib")
                .join("loom-tools.sh"),
            "#!/bin/bash\n# v1 helper (old)",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("lib")
                .join("obsolete.sh"),
            "#!/bin/bash\n# should be removed",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // lib/loom-tools.sh should have new content
        let loom_tools = workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("loom-tools.sh");
        let content = fs::read_to_string(&loom_tools).unwrap();
        assert_eq!(content, "#!/bin/bash\n# v2 helper");

        // Stale file should be removed
        let obsolete = workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("obsolete.sh");
        assert!(!obsolete.exists(), "Stale file in lib/ should be removed on reinstall");

        // Report should track the removal
        assert!(
            report
                .removed
                .contains(&".loom/scripts/lib/obsolete.sh".to_string()),
            "Report should list obsolete.sh as removed, got: {:?}",
            report.removed
        );
    }

    #[test]
    fn test_filter_preserved_from_verification_failures_removes_preserved() {
        // Files preserved by merge strategy must not appear as verification failures
        // (this is the regression case from issue #3218).
        let mut report = InitReport {
            preserved: vec![
                ".claude/settings.json".to_string(),
                ".github/labels.yml".to_string(),
            ],
            verification_failures: vec![
                ".claude/settings.json (content mismatch: source 100 bytes, installed 200 bytes)"
                    .to_string(),
                ".github/labels.yml (content mismatch: source 50 bytes, installed 75 bytes)"
                    .to_string(),
                ".loom/scripts/genuine.sh (content mismatch: source 10 bytes, installed 20 bytes)"
                    .to_string(),
            ],
            ..Default::default()
        };

        filter_preserved_from_verification_failures(&mut report);

        // Only the genuine non-preserved failure should remain
        assert_eq!(report.verification_failures.len(), 1);
        assert!(report.verification_failures[0].contains(".loom/scripts/genuine.sh"));
    }

    #[test]
    fn test_filter_preserved_from_verification_failures_no_preserved() {
        // When nothing is preserved, all failures pass through unchanged
        let mut report = InitReport {
            preserved: vec![],
            verification_failures: vec![".loom/scripts/foo.sh (content mismatch)".to_string()],
            ..Default::default()
        };
        filter_preserved_from_verification_failures(&mut report);
        assert_eq!(report.verification_failures.len(), 1);
    }

    #[test]
    fn test_filter_preserved_from_verification_failures_no_failures() {
        // No-op when there are no failures, even if there are preserved files
        let mut report = InitReport {
            preserved: vec![".claude/settings.json".to_string()],
            verification_failures: vec![],
            ..Default::default()
        };
        filter_preserved_from_verification_failures(&mut report);
        assert!(report.verification_failures.is_empty());
    }

    #[test]
    fn test_preserved_files_excluded_from_verification_failures_end_to_end() {
        // End-to-end: a customized .github/labels.yml (preserved by merge_dir_with_report
        // on reinstall) should not appear as a verification failure after init, even
        // though its bytes differ from the source defaults. This is the regression case
        // from issue #3218.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Minimal defaults
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();

        // .github scaffolding default with content X
        fs::create_dir_all(defaults.join(".github")).unwrap();
        fs::write(defaults.join(".github").join("labels.yml"), "- name: default\n  color: ffffff")
            .unwrap();

        // Pre-existing customized .github/labels.yml with content Y (different bytes)
        fs::create_dir_all(workspace.join(".github")).unwrap();
        fs::write(
            workspace.join(".github").join("labels.yml"),
            "- name: customized\n  color: 000000\n  description: user edit",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok());
        let report = result.unwrap();

        // The customized file must be reported as preserved
        assert!(
            report.preserved.contains(&".github/labels.yml".to_string()),
            "preserved should contain .github/labels.yml, got: {:?}",
            report.preserved
        );

        // And it must NOT appear as a verification failure (the bug we're fixing)
        let leaked: Vec<&String> = report
            .verification_failures
            .iter()
            .filter(|f| f.contains(".github/labels.yml"))
            .collect();
        assert!(
            leaked.is_empty(),
            "preserved file leaked into verification_failures: {:?}",
            report.verification_failures
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_scripts_made_executable_including_subdirectories() {
        // Verifies that make_shell_scripts_executable works recursively
        // on scripts/ and its subdirectories (e.g., scripts/lib/).
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with scripts that are NOT executable
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("scripts").join("worktree.sh"), "#!/bin/bash\n# worktree helper")
            .unwrap();
        fs::write(
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
            "#!/bin/bash\n# shared helper",
        )
        .unwrap();

        // Remove execute bit from source files to simulate git clone stripping perms
        for path in &[
            defaults.join("scripts").join("worktree.sh"),
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
        ] {
            let metadata = fs::metadata(path).unwrap();
            let mut perms = metadata.permissions();
            perms.set_mode(0o644); // rw-r--r-- (no execute)
            fs::set_permissions(path, perms).unwrap();
        }

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());

        // Both scripts should be executable after init
        let worktree_sh = workspace.join(".loom").join("scripts").join("worktree.sh");
        let perms = fs::metadata(&worktree_sh).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "worktree.sh should be executable, mode: {:o}",
            perms.mode()
        );

        let loom_tools = workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("loom-tools.sh");
        let perms = fs::metadata(&loom_tools).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "lib/loom-tools.sh should be executable, mode: {:o}",
            perms.mode()
        );
    }

    #[test]
    fn test_find_missing_files_empty_when_all_present() {
        // Regression test for issue #3220: the post-copy assertion in
        // sync_managed_dir uses find_missing_files to detect partial copies.
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(dst.join("lib")).unwrap();
        fs::write(src.join("a.sh"), "a").unwrap();
        fs::write(dst.join("a.sh"), "a").unwrap();
        fs::write(src.join("lib").join("b.sh"), "b").unwrap();
        fs::write(dst.join("lib").join("b.sh"), "b").unwrap();

        let missing = find_missing_files(&src, &dst);
        assert!(missing.is_empty(), "Expected no missing files, got: {missing:?}");
    }

    #[test]
    fn test_find_missing_files_detects_missing_subdirectory_file() {
        // Specifically verifies the issue #3220 scenario: a file in a
        // subdirectory (e.g., scripts/lib/forge-helpers.sh) is missing
        // from the destination.
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(dst.join("lib")).unwrap();
        fs::write(src.join("a.sh"), "a").unwrap();
        fs::write(dst.join("a.sh"), "a").unwrap();
        fs::write(src.join("lib").join("loom-tools.sh"), "tools").unwrap();
        fs::write(dst.join("lib").join("loom-tools.sh"), "tools").unwrap();
        // Source has forge-helpers.sh but destination does NOT — this is
        // the exact failure mode from issue #3220.
        fs::write(src.join("lib").join("forge-helpers.sh"), "helpers").unwrap();

        let missing = find_missing_files(&src, &dst);
        assert_eq!(missing.len(), 1, "Expected 1 missing file, got: {missing:?}");
        assert_eq!(missing[0], "lib/forge-helpers.sh");
    }

    #[test]
    fn test_find_missing_files_detects_entire_missing_subdir() {
        // If a whole subdirectory is missing, every file under it should be reported.
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("lib").join("a.sh"), "a").unwrap();
        fs::write(src.join("lib").join("b.sh"), "b").unwrap();

        let missing = find_missing_files(&src, &dst);
        assert_eq!(missing.len(), 2, "Expected 2 missing files, got: {missing:?}");
        let mut sorted = missing;
        sorted.sort();
        assert_eq!(sorted, vec!["lib/a.sh".to_string(), "lib/b.sh".to_string()]);
    }
}
