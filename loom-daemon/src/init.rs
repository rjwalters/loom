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
//! 4. Sets up repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
//! 5. Updates .gitignore with Loom ephemeral patterns
//! 6. Reports which files were preserved vs added

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Local;
use serde_json::json;

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
    /// Files that were updated (existed before, overwritten in force mode)
    pub updated: Vec<String>,
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
    /// Whether AGENTS.md exists
    pub has_agents_md: bool,
    /// Whether .github/labels.yml exists
    pub has_labels_yml: bool,
    /// Issues found during validation
    pub issues: Vec<String>,
}

/// Loom installation metadata for template variable substitution
#[derive(Default)]
struct LoomMetadata {
    /// Loom version from `LOOM_VERSION` env var (e.g., "0.1.0")
    version: Option<String>,
    /// Loom commit hash from `LOOM_COMMIT` env var (e.g., "d6cf9ac")
    commit: Option<String>,
    /// Installation date (generated at runtime)
    install_date: String,
}

impl LoomMetadata {
    /// Create metadata by reading from environment variables
    fn from_env() -> Self {
        Self {
            version: std::env::var("LOOM_VERSION").ok(),
            commit: std::env::var("LOOM_COMMIT").ok(),
            install_date: Local::now().format("%Y-%m-%d").to_string(),
        }
    }
}

/// Write version.json to track installed Loom version
///
/// Creates `.loom/version.json` with the current Loom version, commit hash,
/// and installation timestamp. This file is used by the install script to
/// detect whether Loom is already installed at the target version, enabling
/// idempotent installations that skip redundant PR creation.
fn write_version_file(loom_path: &Path, metadata: &LoomMetadata) -> Result<(), String> {
    let version_data = json!({
        "loom_version": metadata.version.as_deref().unwrap_or("unknown"),
        "loom_commit": metadata.commit.as_deref().unwrap_or("unknown"),
        "installed_at": Local::now().to_rfc3339(),
        "installation_mode": "full"
    });

    let version_path = loom_path.join("version.json");
    let content = serde_json::to_string_pretty(&version_data)
        .map_err(|e| format!("Failed to serialize version.json: {e}"))?;

    fs::write(&version_path, format!("{content}\n"))
        .map_err(|e| format!("Failed to write version.json: {e}"))?;

    Ok(())
}

/// Extract repository owner and name from git remote URL
///
/// Parses both HTTPS and SSH git remote URLs to extract owner/repo.
/// Returns None if git remote is not available or URL format is unexpected.
///
/// # Examples
///
/// ```ignore
/// // HTTPS: https://github.com/owner/repo.git -> Some(("owner", "repo"))
/// // SSH: git@github.com:owner/repo.git -> Some(("owner", "repo"))
/// ```
fn extract_repo_info(workspace_path: &Path) -> Option<(String, String)> {
    // Get git remote URL
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let remote_url = String::from_utf8(output.stdout).ok()?;
    let remote_url = remote_url.trim();

    // Parse HTTPS URL: https://github.com/owner/repo.git
    if let Some(https_path) = remote_url.strip_prefix("https://github.com/") {
        let path = https_path.strip_suffix(".git").unwrap_or(https_path);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // Parse SSH URL: git@github.com:owner/repo.git
    if let Some(ssh_path) = remote_url.strip_prefix("git@github.com:") {
        let path = ssh_path.strip_suffix(".git").unwrap_or(ssh_path);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

/// Check if the workspace is the Loom source repository itself
///
/// This detects self-installation to prevent overwriting the source of truth
/// with minimal defaults. When detected, initialization switches to validation-only mode.
///
/// # Detection Methods
///
/// 1. **Marker file**: `.loom-source` file exists (most reliable)
/// 2. **Directory structure**: Has `src-tauri/`, `loom-daemon/`, and `defaults/` directories
/// 3. **Git remote**: Remote URL matches known Loom repositories
///
/// # Returns
///
/// `true` if this appears to be the Loom source repository
pub fn is_loom_source_repo(workspace_path: &Path) -> bool {
    // Method 1: Check for explicit marker file
    if workspace_path.join(".loom-source").exists() {
        return true;
    }

    // Method 2: Check for Loom-specific directory structure
    let has_src_tauri = workspace_path.join("src-tauri").is_dir();
    let has_loom_daemon = workspace_path.join("loom-daemon").is_dir();
    let has_defaults = workspace_path.join("defaults").is_dir();

    if has_src_tauri && has_loom_daemon && has_defaults {
        // Additional check: defaults should have config.json and roles/
        let defaults_has_config = workspace_path.join("defaults").join("config.json").exists();
        let defaults_has_roles = workspace_path.join("defaults").join("roles").is_dir();

        if defaults_has_config && defaults_has_roles {
            return true;
        }
    }

    // Method 3: Check git remote for known Loom repositories
    if let Some((owner, repo)) = extract_repo_info(workspace_path) {
        // Match various Loom repository locations
        let is_loom_repo = (owner == "rjwalters" && repo == "loom")
            || repo == "loom" && workspace_path.join("src-tauri").is_dir();

        if is_loom_repo {
            return true;
        }
    }

    false
}

/// Validate an existing Loom source repository configuration
///
/// Instead of copying files, this validates that the expected structure exists
/// and reports any issues found.
fn validate_loom_source_repo(workspace_path: &Path) -> ValidationReport {
    let mut report = ValidationReport::default();
    let loom_path = workspace_path.join(".loom");

    // Check roles
    let roles_dir = loom_path.join("roles");
    if roles_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&roles_dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    if let Some(name) = path.file_stem() {
                        report.roles_found.push(name.to_string_lossy().to_string());
                    }
                }
            }
        }
    } else {
        report
            .issues
            .push("Missing .loom/roles/ directory".to_string());
    }

    // Check scripts
    let scripts_dir = loom_path.join("scripts");
    if scripts_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&scripts_dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "sh") {
                    if let Some(name) = path.file_stem() {
                        report
                            .scripts_found
                            .push(name.to_string_lossy().to_string());
                    }
                }
            }
        }
    } else {
        report
            .issues
            .push("Missing .loom/scripts/ directory".to_string());
    }

    // Check .claude/commands/
    let commands_dir = workspace_path.join(".claude").join("commands");
    if commands_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&commands_dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    if let Some(name) = path.file_stem() {
                        report
                            .commands_found
                            .push(name.to_string_lossy().to_string());
                    }
                }
            }
        }
    } else {
        report
            .issues
            .push("Missing .claude/commands/ directory".to_string());
    }

    // Check documentation files
    report.has_claude_md = workspace_path.join("CLAUDE.md").exists();
    if !report.has_claude_md {
        report.issues.push("Missing CLAUDE.md".to_string());
    }

    report.has_agents_md = workspace_path.join("AGENTS.md").exists();
    if !report.has_agents_md {
        report.issues.push("Missing AGENTS.md".to_string());
    }

    // Check labels.yml
    report.has_labels_yml = workspace_path.join(".github").join("labels.yml").exists();
    if !report.has_labels_yml {
        report.issues.push("Missing .github/labels.yml".to_string());
    }

    // Validate minimum expected counts
    if report.roles_found.len() < 5 {
        report.issues.push(format!(
            "Expected at least 5 role definitions, found {}",
            report.roles_found.len()
        ));
    }

    if report.scripts_found.len() < 2 {
        report
            .issues
            .push(format!("Expected at least 2 scripts, found {}", report.scripts_found.len()));
    }

    if report.commands_found.len() < 4 {
        report.issues.push(format!(
            "Expected at least 4 slash commands, found {}",
            report.commands_found.len()
        ));
    }

    report
}

/// Replace template variables in a string
///
/// Replaces the following template variables:
/// - `{{REPO_OWNER}}`: Repository owner from git remote
/// - `{{REPO_NAME}}`: Repository name from git remote
/// - `{{LOOM_VERSION}}`: Loom version from environment
/// - `{{LOOM_COMMIT}}`: Loom commit hash from environment
/// - `{{INSTALL_DATE}}`: Current date (YYYY-MM-DD format)
///
/// If repo info is not available (non-GitHub remote or no remote),
/// falls back to generic placeholders. If Loom metadata is not available,
/// falls back to "unknown" placeholders.
fn substitute_template_variables(
    content: &str,
    repo_owner: Option<&str>,
    repo_name: Option<&str>,
    loom_metadata: &LoomMetadata,
) -> String {
    let owner = repo_owner.unwrap_or("OWNER");
    let name = repo_name.unwrap_or("REPO");
    let version = loom_metadata.version.as_deref().unwrap_or("unknown");
    let commit = loom_metadata.commit.as_deref().unwrap_or("unknown");

    content
        .replace("{{REPO_OWNER}}", owner)
        .replace("{{REPO_NAME}}", name)
        .replace("{{LOOM_VERSION}}", version)
        .replace("{{LOOM_COMMIT}}", commit)
        .replace("{{INSTALL_DATE}}", &loom_metadata.install_date)
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

        // Run validation instead of copying files
        let validation = validate_loom_source_repo(workspace);
        report.validation = Some(validation);

        // Update .gitignore (this is safe even for self-install)
        update_gitignore(workspace)?;

        return Ok(report);
    }

    // Resolve defaults path (development mode or bundled resource)
    let defaults = resolve_defaults_path(defaults_path)?;

    // Determine if this is a fresh install or reinstall
    let is_reinstall = loom_path.exists();

    // Note: We no longer delete .loom directory in force mode.
    // Force mode now means "overwrite default files but preserve custom files"
    // This allows reinstallation to update Loom while keeping project-specific roles.
    let _ = (is_reinstall, force); // Silence unused warning, these affect behavior below

    // Create .loom directory if it doesn't exist
    fs::create_dir_all(&loom_path).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy config.json (always overwrite - this is Loom's config, not user data)
    let config_src = defaults.join("config.json");
    let config_dst = loom_path.join("config.json");
    if config_src.exists() {
        let existed = config_dst.exists();
        fs::copy(&config_src, &config_dst)
            .map_err(|e| format!("Failed to copy config.json: {e}"))?;
        if existed {
            report.updated.push(".loom/config.json".to_string());
        } else {
            report.added.push(".loom/config.json".to_string());
        }
    }

    // Copy roles/ directory - always update default roles, preserve custom roles
    // - Fresh install: copy all from defaults
    // - Reinstall: always force-merge (update defaults, preserve custom)
    //
    // This ensures role updates from loom propagate to target repos while
    // preserving any custom roles the project has added.
    let roles_src = defaults.join("roles");
    let roles_dst = loom_path.join("roles");
    if roles_src.exists() {
        if !is_reinstall {
            // Fresh install: copy all
            copy_dir_with_report(&roles_src, &roles_dst, ".loom/roles", &mut report)
                .map_err(|e| format!("Failed to copy roles directory: {e}"))?;
        } else {
            // Reinstall: always force-merge to update default roles
            // Custom roles (files not in defaults) are preserved
            force_merge_dir_with_report(&roles_src, &roles_dst, ".loom/roles", &mut report)
                .map_err(|e| format!("Failed to force-merge roles directory: {e}"))?;
        }
    }

    // Copy scripts/ directory - always update default scripts, preserve custom scripts
    // Same logic as roles
    let scripts_src = defaults.join("scripts");
    let scripts_dst = loom_path.join("scripts");
    if scripts_src.exists() {
        if !is_reinstall {
            // Fresh install: copy all
            copy_dir_with_report(&scripts_src, &scripts_dst, ".loom/scripts", &mut report)
                .map_err(|e| format!("Failed to copy scripts directory: {e}"))?;
        } else {
            // Reinstall: always force-merge to update default scripts
            // Custom scripts (files not in defaults) are preserved
            force_merge_dir_with_report(&scripts_src, &scripts_dst, ".loom/scripts", &mut report)
                .map_err(|e| format!("Failed to force-merge scripts directory: {e}"))?;
        }
    }

    // Copy .loom-specific README
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        let existed = loom_readme_dst.exists();
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
        if existed {
            report.updated.push(".loom/README.md".to_string());
        } else {
            report.added.push(".loom/README.md".to_string());
        }
    }

    // Update .gitignore with Loom ephemeral patterns
    update_gitignore(workspace)?;

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace, &defaults, force, &mut report)?;

    // Write version.json to track installed version (enables idempotent installs)
    let loom_metadata = LoomMetadata::from_env();
    write_version_file(&loom_path, &loom_metadata)?;

    Ok(report)
}

/// Validate that a path is a git repository
///
/// Checks that the path exists, is a directory, and contains a .git directory.
fn validate_git_repository(path: &str) -> Result<(), String> {
    let workspace_path = Path::new(path);

    // Check if the path exists
    if !workspace_path.exists() {
        return Err(format!("Path does not exist: {path}"));
    }

    // Check if it's a directory
    if !workspace_path.is_dir() {
        return Err(format!("Path is not a directory: {path}"));
    }

    // Check for .git directory
    let git_path = workspace_path.join(".git");
    if !git_path.exists() {
        return Err(format!("Not a git repository (no .git directory found): {path}"));
    }

    Ok(())
}

/// Helper function to find git repository root by searching for .git directory
fn find_git_root() -> Option<PathBuf> {
    // Start from current directory
    let mut current = std::env::current_dir().ok()?;

    loop {
        let git_dir = current.join(".git");

        // Security: Check if .git exists and is NOT a symlink
        // Prevents symlink-based directory traversal attacks (CWE-59)
        if git_dir.exists() {
            if let Ok(metadata) = git_dir.symlink_metadata() {
                if metadata.is_symlink() {
                    // Reject symlinks to prevent directory escape
                    return None;
                }
            }
            return Some(current);
        }

        // Move up to parent directory
        if !current.pop() {
            // Reached filesystem root without finding .git
            return None;
        }
    }
}

/// Resolve defaults directory path
///
/// Tries development path first, then falls back to bundled resource path.
/// This handles both development mode and production builds.
///
/// # Search Order
///
/// 1. Provided path (development mode - relative to cwd)
/// 2. Git repository root + path (handles git worktrees)
/// 3. Bundled resource path (production mode - .app/Contents/Resources/)
fn resolve_defaults_path(defaults_path: &str) -> Result<PathBuf, String> {
    let mut tried_paths = Vec::new();

    // Try the provided path first (development mode - relative to cwd)
    let dev_path = PathBuf::from(defaults_path);
    tried_paths.push(dev_path.display().to_string());
    if dev_path.exists() {
        return Ok(dev_path);
    }

    // Try finding defaults relative to git repository root
    // This handles the case where we're running from a git worktree
    if let Some(git_root) = find_git_root() {
        let git_root_defaults = git_root.join(defaults_path);
        tried_paths.push(git_root_defaults.display().to_string());
        if git_root_defaults.exists() {
            return Ok(git_root_defaults);
        }
    }

    // Try resolving as bundled resource (production mode)
    // In production, resources are in .app/Contents/Resources/ on macOS
    if let Ok(exe_path) = std::env::current_exe() {
        // Get the app bundle Resources directory
        if let Some(exe_dir) = exe_path.parent() {
            // exe is in Contents/MacOS/, resources are in Contents/Resources/
            if let Some(contents_dir) = exe_dir.parent() {
                let resources_dir = contents_dir.join("Resources");

                // Try with _up_ prefix (Tauri bundles ../defaults as _up_/defaults)
                let up_path = resources_dir.join("_up_").join(defaults_path);
                tried_paths.push(up_path.display().to_string());
                if up_path.exists() {
                    return Ok(up_path);
                }

                // Try with subdirectory name (standard Tauri bundling)
                let resources_path = resources_dir.join(defaults_path);
                tried_paths.push(resources_path.display().to_string());
                if resources_path.exists() {
                    return Ok(resources_path);
                }

                // Try the Resources directory itself (in case bundling flattens structure)
                tried_paths.push(resources_dir.display().to_string());
                if resources_dir.join("config.json").exists() {
                    return Ok(resources_dir);
                }
            }
        }
    }

    Err(format!(
        "Defaults directory not found. Tried paths:\n  {}",
        tried_paths.join("\n  ")
    ))
}

/// Copy directory recursively
///
/// Creates the destination directory and copies all files and subdirectories.
#[cfg(test)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Merge directory recursively
///
/// Copies files from src to dst, but only if they don't already exist in dst.
/// This allows merging new files from defaults while preserving existing customizations.
#[cfg(test)]
fn merge_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            // Recursively merge subdirectories
            merge_dir_recursive(&src_path, &dst_path)?;
        } else if !dst_path.exists() {
            // Only copy if destination file doesn't exist
            fs::copy(&src_path, &dst_path)?;
        }
        // If dst_path exists, skip (preserve existing file)
    }

    Ok(())
}

/// Copy directory recursively with reporting
///
/// Like `copy_dir_recursive` but tracks which files were added.
fn copy_dir_with_report(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            copy_dir_with_report(&src_path, &dst_path, &rel_path, report)?;
        } else {
            let existed = dst_path.exists();
            fs::copy(&src_path, &dst_path)?;
            if existed {
                report.updated.push(rel_path);
            } else {
                report.added.push(rel_path);
            }
        }
    }

    Ok(())
}

/// Merge directory recursively with reporting
///
/// Like `merge_dir_recursive` but tracks which files were added vs preserved.
fn merge_dir_with_report(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    // Collect files in source for comparison
    let src_files: HashSet<_> = fs::read_dir(src)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name())
        .collect();

    // Check for files in dst that aren't in src (custom files)
    if dst.exists() {
        for entry in fs::read_dir(dst)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if !src_files.contains(&file_name) {
                let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());
                // This is a custom file not in defaults - preserved
                if entry.file_type()?.is_file() {
                    report.preserved.push(rel_path);
                }
            }
        }
    }

    // Process source files
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            merge_dir_with_report(&src_path, &dst_path, &rel_path, report)?;
        } else if dst_path.exists() {
            // File exists - preserve it (don't overwrite)
            report.preserved.push(rel_path);
        } else {
            // File doesn't exist - add it
            fs::copy(&src_path, &dst_path)?;
            report.added.push(rel_path);
        }
    }

    Ok(())
}

/// Force-merge directory recursively with reporting
///
/// Like `merge_dir_with_report` but OVERWRITES files from defaults while
/// still preserving custom files (files in dst that don't exist in src).
/// This is used for reinstallation to update Loom files while keeping
/// project-specific customizations.
fn force_merge_dir_with_report(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    // Collect files in source for comparison
    let src_files: HashSet<_> = fs::read_dir(src)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name())
        .collect();

    // Check for files in dst that aren't in src (custom files) - these are preserved
    if dst.exists() {
        for entry in fs::read_dir(dst)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if !src_files.contains(&file_name) {
                let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());
                // This is a custom file not in defaults - preserved
                if entry.file_type()?.is_file() {
                    report.preserved.push(rel_path);
                }
            }
        }
    }

    // Process source files - overwrite existing files from defaults
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            force_merge_dir_with_report(&src_path, &dst_path, &rel_path, report)?;
        } else {
            let existed = dst_path.exists();
            // Always copy from source (overwrite if exists)
            fs::copy(&src_path, &dst_path)?;
            if existed {
                report.updated.push(rel_path);
            } else {
                report.added.push(rel_path);
            }
        }
    }

    Ok(())
}

/// Loom section markers for CLAUDE.md content preservation
const LOOM_SECTION_START: &str = "<!-- BEGIN LOOM ORCHESTRATION -->";
const LOOM_SECTION_END: &str = "<!-- END LOOM ORCHESTRATION -->";

/// Wrap Loom content in section markers
fn wrap_loom_content(content: &str) -> String {
    format!("{}\n{}\n{}", LOOM_SECTION_START, content.trim(), LOOM_SECTION_END)
}

/// Setup repository scaffolding files
///
/// Copies CLAUDE.md, AGENTS.md, .claude/, .codex/, and .github/ to the workspace.
/// - Fresh install: Copies all files from defaults
/// - Reinstall without force (merge mode): Adds new files, preserves ALL existing files
/// - Reinstall with force (force-merge mode): Updates default files, preserves custom files
/// - Template variables: Substitutes variables in CLAUDE.md, AGENTS.md, and workflow files
///   - `{{REPO_OWNER}}`, `{{REPO_NAME}}`: Repository info from git remote
///   - `{{LOOM_VERSION}}`, `{{LOOM_COMMIT}}`, `{{INSTALL_DATE}}`: Loom installation metadata
///
/// **CLAUDE.md Preservation**:
/// - If existing CLAUDE.md has Loom section markers, only the marked section is replaced
/// - If existing CLAUDE.md has no markers, Loom section is appended at the end
/// - Loom content is wrapped in `<!-- BEGIN LOOM ORCHESTRATION -->` markers
/// - All existing content is preserved exactly as-is
///
/// Custom files (files in workspace that don't exist in defaults) are always preserved.
#[allow(clippy::too_many_lines)]
fn setup_repository_scaffolding(
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

    // Helper to copy file with template variable substitution
    let copy_file_with_substitution =
        |src: &Path, dst: &Path, name: &str, report: &mut InitReport| -> Result<(), String> {
            if src.exists() {
                let existed = dst.exists();
                if force && existed {
                    fs::remove_file(dst)
                        .map_err(|e| format!("Failed to remove existing {name}: {e}"))?;
                }
                if force || !existed {
                    let content = fs::read_to_string(src)
                        .map_err(|e| format!("Failed to read {name}: {e}"))?;
                    let substituted = substitute_template_variables(
                        &content,
                        repo_owner.as_deref(),
                        repo_name.as_deref(),
                        &loom_metadata,
                    );
                    fs::write(dst, substituted)
                        .map_err(|e| format!("Failed to write {name}: {e}"))?;
                    if existed {
                        report.updated.push(name.to_string());
                    } else {
                        report.added.push(name.to_string());
                    }
                } else {
                    report.preserved.push(name.to_string());
                }
            }
            Ok(())
        };

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

    // Copy target-repo-specific CLAUDE.md from defaults/.loom/
    // (NOT defaults/CLAUDE.md which is for Loom repo itself)
    // This file contains template variables that need to be substituted
    //
    // CLAUDE.md Preservation Logic:
    // - If existing CLAUDE.md has section markers, replace only the marked section
    // - If existing CLAUDE.md has no markers, append Loom section at the end
    // - Loom content is wrapped in section markers for future updates
    // - All existing content is preserved exactly as-is
    let claude_md_src = defaults_path.join(".loom").join("CLAUDE.md");
    let claude_md_dst = workspace_path.join("CLAUDE.md");

    if claude_md_src.exists() {
        let existed = claude_md_dst.exists();

        // Read the new Loom template content
        let loom_content = fs::read_to_string(&claude_md_src)
            .map_err(|e| format!("Failed to read CLAUDE.md template: {e}"))?;

        // Substitute template variables in Loom content
        let loom_substituted = substitute_template_variables(
            &loom_content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        // Wrap Loom content in section markers
        let wrapped_loom = wrap_loom_content(&loom_substituted);

        let final_content = if existed {
            // Read existing content
            let existing_content = fs::read_to_string(&claude_md_dst)
                .map_err(|e| format!("Failed to read existing CLAUDE.md: {e}"))?;

            // Check if existing file already has Loom section markers
            if existing_content.contains(LOOM_SECTION_START) {
                // Replace just the Loom section, preserve everything else
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

                    format!("{}{}{}", before.trim_end(), wrapped_loom, after)
                } else {
                    // Malformed markers - append at end
                    format!("{}\n\n{}", existing_content.trim(), wrapped_loom)
                }
            } else {
                // No markers exist - append Loom section at end
                format!("{}\n\n{}", existing_content.trim(), wrapped_loom)
            }
        } else {
            // New file - just use wrapped Loom content
            wrapped_loom
        };

        // Only write if we're creating new or updating
        if !existed {
            fs::write(&claude_md_dst, &final_content)
                .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
            report.added.push("CLAUDE.md".to_string());
        } else if force || final_content != fs::read_to_string(&claude_md_dst).unwrap_or_default() {
            // Check if content actually changed to avoid unnecessary writes
            let current = fs::read_to_string(&claude_md_dst).unwrap_or_default();
            if final_content != current {
                fs::write(&claude_md_dst, &final_content)
                    .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
                if existed && !report.preserved.contains(&"CLAUDE.md".to_string()) {
                    report.updated.push("CLAUDE.md".to_string());
                }
            } else if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                report.preserved.push("CLAUDE.md".to_string());
            }
        }
    }

    copy_file_with_substitution(
        &defaults_path.join(".loom").join("AGENTS.md"),
        &workspace_path.join("AGENTS.md"),
        "AGENTS.md",
        report,
    )?;

    // Copy .claude/ directory - always update default commands, preserve custom commands
    // - Fresh install: copy all from defaults
    // - Reinstall: always force-merge (update defaults, preserve custom)
    //
    // This ensures command updates from loom propagate to target repos while
    // preserving any custom commands the project has added.
    // Consistent with .loom/roles/ and .loom/scripts/ behavior.
    let claude_src = defaults_path.join(".claude");
    let claude_dst = workspace_path.join(".claude");
    if claude_src.exists() {
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

    // Process workflow files with template variable substitution
    let workflow_file = workspace_path
        .join(".github")
        .join("workflows")
        .join("label-external-issues.yml");

    if workflow_file.exists() {
        let content = fs::read_to_string(&workflow_file)
            .map_err(|e| format!("Failed to read workflow file: {e}"))?;

        let substituted = substitute_template_variables(
            &content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        fs::write(&workflow_file, substituted)
            .map_err(|e| format!("Failed to write workflow file: {e}"))?;
    }

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

    Ok(())
}

/// Update .gitignore with Loom ephemeral patterns
///
/// Adds patterns for ephemeral Loom files that shouldn't be committed.
/// Creates .gitignore if it doesn't exist.
fn update_gitignore(workspace_path: &Path) -> Result<(), String> {
    let gitignore_path = workspace_path.join(".gitignore");

    // Ephemeral files that should be ignored
    let ephemeral_patterns = [
        ".loom/state.json",
        ".loom/version.json",
        ".loom/worktrees/",
        ".loom/*.log",
        ".loom/*.sock",
    ];

    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

        let mut new_contents = contents.clone();
        let mut modified = false;

        // Add ephemeral patterns if not present
        for pattern in &ephemeral_patterns {
            if !contents.contains(pattern) {
                if !new_contents.ends_with('\n') {
                    new_contents.push('\n');
                }
                new_contents.push_str(pattern);
                new_contents.push('\n');
                modified = true;
            }
        }

        // Write back if we made changes
        if modified {
            fs::write(&gitignore_path, new_contents)
                .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
        }
    } else {
        // Create .gitignore with ephemeral patterns
        let mut loom_entries = String::from("# Loom - AI Development Orchestration\n");
        for pattern in &ephemeral_patterns {
            loom_entries.push_str(pattern);
            loom_entries.push('\n');
        }
        fs::write(&gitignore_path, loom_entries)
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_validate_git_repository() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Not a git repo yet
        let result = validate_git_repository(workspace.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not a git repository"));

        // Create .git directory
        fs::create_dir(workspace.join(".git")).unwrap();

        // Should now be valid
        let result = validate_git_repository(workspace.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn test_substitute_template_variables() {
        let content = r"
**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}
**Repository**: {{REPO_OWNER}}/{{REPO_NAME}}
";

        // Test with all values provided
        let metadata = LoomMetadata {
            version: Some("1.2.3".to_string()),
            commit: Some("abc1234".to_string()),
            install_date: "2024-01-15".to_string(),
        };

        let result =
            substitute_template_variables(content, Some("myorg"), Some("myrepo"), &metadata);

        assert!(result.contains("**Loom Version**: 1.2.3"));
        assert!(result.contains("**Loom Commit**: abc1234"));
        assert!(result.contains("**Installation Date**: 2024-01-15"));
        assert!(result.contains("**Repository**: myorg/myrepo"));

        // Test with missing values (should use fallbacks)
        let metadata_empty = LoomMetadata {
            version: None,
            commit: None,
            install_date: "2024-01-15".to_string(),
        };

        let result_fallback = substitute_template_variables(content, None, None, &metadata_empty);

        assert!(result_fallback.contains("**Loom Version**: unknown"));
        assert!(result_fallback.contains("**Loom Commit**: unknown"));
        assert!(result_fallback.contains("**Repository**: OWNER/REPO"));
    }

    #[test]
    fn test_loom_metadata_from_env() {
        // Test with environment variables set
        std::env::set_var("LOOM_VERSION", "0.5.0");
        std::env::set_var("LOOM_COMMIT", "def5678");

        let metadata = LoomMetadata::from_env();

        assert_eq!(metadata.version, Some("0.5.0".to_string()));
        assert_eq!(metadata.commit, Some("def5678".to_string()));
        // install_date should be today's date in YYYY-MM-DD format
        assert!(metadata.install_date.len() == 10);
        assert!(metadata.install_date.contains('-'));

        // Clean up
        std::env::remove_var("LOOM_VERSION");
        std::env::remove_var("LOOM_COMMIT");
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_copy_dir_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        // Create source structure
        fs::create_dir(&src).unwrap();
        fs::write(src.join("file1.txt"), "content1").unwrap();
        fs::create_dir(src.join("subdir")).unwrap();
        fs::write(src.join("subdir").join("file2.txt"), "content2").unwrap();

        // Copy recursively
        copy_dir_recursive(&src, &dst).unwrap();

        // Verify destination
        assert!(dst.exists());
        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("subdir").exists());
        assert!(dst.join("subdir").join("file2.txt").exists());

        let content1 = fs::read_to_string(dst.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(dst.join("subdir").join("file2.txt")).unwrap();
        assert_eq!(content2, "content2");
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_merge_dir_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        // Create source structure
        fs::create_dir(&src).unwrap();
        fs::write(src.join("file1.txt"), "new content 1").unwrap();
        fs::write(src.join("file2.txt"), "new content 2").unwrap();
        fs::create_dir(src.join("subdir")).unwrap();
        fs::write(src.join("subdir").join("file3.txt"), "new content 3").unwrap();

        // Create destination with existing file
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("file1.txt"), "existing content").unwrap();
        fs::write(dst.join("existing.txt"), "preserve me").unwrap();

        // Merge directories
        merge_dir_recursive(&src, &dst).unwrap();

        // Verify: file1.txt should be preserved (not overwritten)
        let content1 = fs::read_to_string(dst.join("file1.txt")).unwrap();
        assert_eq!(content1, "existing content");

        // Verify: file2.txt should be copied (new file)
        assert!(dst.join("file2.txt").exists());
        let content2 = fs::read_to_string(dst.join("file2.txt")).unwrap();
        assert_eq!(content2, "new content 2");

        // Verify: subdir and file3.txt should be created
        assert!(dst.join("subdir").exists());
        assert!(dst.join("subdir").join("file3.txt").exists());
        let content3 = fs::read_to_string(dst.join("subdir").join("file3.txt")).unwrap();
        assert_eq!(content3, "new content 3");

        // Verify: existing.txt should still exist
        assert!(dst.join("existing.txt").exists());
        let existing = fs::read_to_string(dst.join("existing.txt")).unwrap();
        assert_eq!(existing, "preserve me");
    }

    #[test]
    #[allow(clippy::unwrap_used)]
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
    #[allow(clippy::unwrap_used)]
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
    #[allow(clippy::unwrap_used)]
    fn test_force_merge_preserves_custom_files() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        // Create source (defaults) with some files
        fs::create_dir(&src).unwrap();
        fs::write(src.join("builder.md"), "new builder content").unwrap();
        fs::write(src.join("judge.md"), "new judge content").unwrap();

        // Create destination with existing files (some default, some custom)
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("builder.md"), "old builder content").unwrap();
        fs::write(dst.join("designer.md"), "custom designer content").unwrap(); // Custom file

        // Force-merge directories
        let mut report = InitReport::default();
        force_merge_dir_with_report(&src, &dst, "roles", &mut report).unwrap();

        // Verify: builder.md was UPDATED (overwritten from defaults)
        let builder = fs::read_to_string(dst.join("builder.md")).unwrap();
        assert_eq!(builder, "new builder content");

        // Verify: judge.md was ADDED (new file from defaults)
        let judge = fs::read_to_string(dst.join("judge.md")).unwrap();
        assert_eq!(judge, "new judge content");

        // Verify: designer.md was PRESERVED (custom file not in defaults)
        assert!(dst.join("designer.md").exists());
        let designer = fs::read_to_string(dst.join("designer.md")).unwrap();
        assert_eq!(designer, "custom designer content");

        // Verify report
        assert!(report.updated.contains(&"roles/builder.md".to_string()));
        assert!(report.added.contains(&"roles/judge.md".to_string()));
        assert!(report.preserved.contains(&"roles/designer.md".to_string()));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_roles_always_updated_on_reinstall() {
        // Roles should always be force-merged on reinstall (without --force flag)
        // This ensures role updates propagate while custom roles are preserved
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
        fs::write(workspace.join(".loom").join("roles").join("custom-role.md"), "my custom role")
            .unwrap();

        // Run initialization WITHOUT force flag (simulates normal reinstall)
        let result = initialize_workspace(
            workspace.to_str().unwrap(),
            defaults.to_str().unwrap(),
            false, // No force flag
        );

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify: builder.md was UPDATED (default role updated)
        let builder =
            fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
        assert_eq!(builder, "new builder content v2");

        // Verify: judge.md was ADDED (new default role)
        let judge =
            fs::read_to_string(workspace.join(".loom").join("roles").join("judge.md")).unwrap();
        assert_eq!(judge, "new judge content");

        // Verify: custom-role.md was PRESERVED (custom role not in defaults)
        let custom =
            fs::read_to_string(workspace.join(".loom").join("roles").join("custom-role.md"))
                .unwrap();
        assert_eq!(custom, "my custom role");

        // Verify report reflects the changes
        assert!(report
            .updated
            .contains(&".loom/roles/builder.md".to_string()));
        assert!(report.added.contains(&".loom/roles/judge.md".to_string()));
        assert!(report
            .preserved
            .contains(&".loom/roles/custom-role.md".to_string()));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_claude_commands_always_updated_on_reinstall() {
        // .claude/ commands should always be force-merged on reinstall (without --force flag)
        // This ensures command updates propagate while custom commands are preserved
        // Mirrors test_roles_always_updated_on_reinstall but for .claude/ directory
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
        let builder_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("builder.md"))
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

    #[test]
    #[allow(clippy::unwrap_used)]
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
    #[allow(clippy::unwrap_used)]
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

    #[test]
    #[allow(clippy::unwrap_used)]
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
    #[allow(clippy::unwrap_used)]
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
    #[allow(clippy::unwrap_used)]
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
        fs::write(workspace.join("AGENTS.md"), "").unwrap();

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
        assert!(validation.has_agents_md);
        assert!(validation.has_labels_yml);
    }

    #[test]
    fn test_wrap_loom_content() {
        let content = "# Loom Orchestration\n\nLoom content here.";
        let wrapped = wrap_loom_content(content);

        assert!(wrapped.starts_with(LOOM_SECTION_START));
        assert!(wrapped.ends_with(LOOM_SECTION_END));
        assert!(wrapped.contains("Loom content here"));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
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

        // No existing CLAUDE.md in workspace
        assert!(!workspace.join("CLAUDE.md").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify CLAUDE.md was created with section markers
        assert!(workspace.join("CLAUDE.md").exists());
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains("Loom Orchestration"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
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

        // Create existing CLAUDE.md with project-specific content (no markers)
        fs::write(
            workspace.join("CLAUDE.md"),
            r#"# My Awesome Project

This project does amazing things with Rust.

## Getting Started

Run `cargo run` to start."#,
        )
        .unwrap();

        // Run setup - Loom section should be appended at end
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify existing content was preserved and Loom section appended
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Awesome Project"));
        assert!(content.contains("amazing things with Rust"));
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains("Loom Orchestration"));

        // Project content should come BEFORE Loom section (appended at end)
        let project_pos = content.find("My Awesome Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // No duplicate content
        assert_eq!(content.matches("My Awesome Project").count(), 1);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
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

        // Create existing CLAUDE.md WITHOUT markers (e.g., from previous install or manual creation)
        fs::write(
            workspace.join("CLAUDE.md"),
            r#"# Lean Genius Project

Formal mathematics in Lean 4.

## Docker Build Safety

WARNING: Never run `lake build` inside Docker - causes memory corruption.

## Custom Agents

- Erdos: Mathematical proof orchestrator
- Aristotle: Automated theorem prover"#,
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

        // Verify Loom section was appended at end with markers
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains("Loom Orchestration"));

        // Verify order: project content comes BEFORE Loom section
        let project_pos = content.find("Lean Genius Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // Verify no duplicate content or mangling
        assert_eq!(content.matches("Lean Genius Project").count(), 1);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_claude_md_preservation_update_loom_section_only() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with NEW CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nUPDATED Loom content v2.0.",
        )
        .unwrap();

        // Create existing CLAUDE.md with markers (previous install)
        let existing = format!(
            r#"# My Project

Project docs here.

{}
# Loom Orchestration - Repository Guide

Old Loom content v1.0.
{}"#,
            LOOM_SECTION_START, LOOM_SECTION_END
        );
        fs::write(workspace.join("CLAUDE.md"), existing).unwrap();

        // Run setup with force=true
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify project content was preserved, Loom section was updated
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Project"));
        assert!(content.contains("Project docs here"));
        assert!(content.contains("UPDATED Loom content v2.0"));
        assert!(!content.contains("Old Loom content v1.0"));

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
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_write_version_file() {
        let temp_dir = TempDir::new().unwrap();
        let loom_path = temp_dir.path().join(".loom");
        fs::create_dir_all(&loom_path).unwrap();

        let metadata = LoomMetadata {
            version: Some("1.0.0".to_string()),
            commit: Some("abc1234".to_string()),
            install_date: "2026-01-26".to_string(),
        };

        write_version_file(&loom_path, &metadata).unwrap();

        let version_path = loom_path.join("version.json");
        assert!(version_path.exists());

        let content = fs::read_to_string(&version_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed["loom_version"], "1.0.0");
        assert_eq!(parsed["loom_commit"], "abc1234");
        assert_eq!(parsed["installation_mode"], "full");
        assert!(parsed["installed_at"].is_string());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_version_file_created_during_init() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create minimal defaults
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();

        // Set env vars for version tracking
        std::env::set_var("LOOM_VERSION", "2.0.0");
        std::env::set_var("LOOM_COMMIT", "def5678");

        let result = initialize_workspace(
            workspace.to_str().unwrap(),
            defaults.to_str().unwrap(),
            false,
        );

        assert!(result.is_ok());

        // Verify version.json was created
        let version_path = workspace.join(".loom").join("version.json");
        assert!(version_path.exists());

        let content = fs::read_to_string(&version_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["loom_version"], "2.0.0");
        assert_eq!(parsed["loom_commit"], "def5678");

        // Clean up
        std::env::remove_var("LOOM_VERSION");
        std::env::remove_var("LOOM_COMMIT");
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_gitignore_includes_version_json() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Create empty .gitignore
        fs::write(workspace.join(".gitignore"), "").unwrap();

        update_gitignore(workspace).unwrap();

        let content = fs::read_to_string(workspace.join(".gitignore")).unwrap();
        assert!(
            content.contains(".loom/version.json"),
            "gitignore should include .loom/version.json"
        );
    }
}
