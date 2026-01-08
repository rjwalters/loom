//! Loom workspace initialization module
//!
//! This module provides functionality for initializing Loom workspaces
//! without requiring the Tauri application. It can be used from:
//! - CLI mode (loom-daemon init)
//! - Tauri IPC commands (shared code)
//!
//! The initialization process:
//! 1. Validates the target is a git repository
//! 2. Copies `.loom/` configuration from `defaults/`
//! 3. Sets up repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
//! 4. Updates .gitignore with Loom ephemeral patterns

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Local;

/// Loom installation metadata for template variable substitution
#[derive(Default)]
struct LoomMetadata {
    /// Loom version from LOOM_VERSION env var (e.g., "0.1.0")
    version: Option<String>,
    /// Loom commit hash from LOOM_COMMIT env var (e.g., "d6cf9ac")
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
/// * `force` - If true, overwrite existing `.loom` directory
///
/// # Returns
///
/// * `Ok(())` - Workspace successfully initialized
/// * `Err(String)` - Initialization failed with error message
///
/// # Errors
///
/// This function will return an error if:
/// - The workspace path doesn't exist or isn't a directory
/// - The workspace isn't a git repository (no .git directory)
/// - The `.loom` directory already exists (unless `force` is true)
/// - File operations fail (insufficient permissions, disk full, etc.)
pub fn initialize_workspace(
    workspace_path: &str,
    defaults_path: &str,
    force: bool,
) -> Result<(), String> {
    let workspace = Path::new(workspace_path);
    let loom_path = workspace.join(".loom");

    // Validate workspace is a git repository
    validate_git_repository(workspace_path)?;

    // Check if .loom already exists
    if loom_path.exists() && !force {
        return Err(
            "Workspace already initialized (.loom directory exists). Use --force to overwrite."
                .to_string(),
        );
    }

    // Remove existing .loom if force mode
    if loom_path.exists() && force {
        fs::remove_dir_all(&loom_path)
            .map_err(|e| format!("Failed to remove existing .loom directory: {e}"))?;
    }

    // Resolve defaults path (development mode or bundled resource)
    let defaults = resolve_defaults_path(defaults_path)?;

    // Create .loom directory
    fs::create_dir_all(&loom_path).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy only .loom-specific files from defaults
    // (NOT entire defaults/ tree which would create duplicates)

    // Copy config.json
    let config_src = defaults.join("config.json");
    let config_dst = loom_path.join("config.json");
    if config_src.exists() {
        fs::copy(&config_src, &config_dst)
            .map_err(|e| format!("Failed to copy config.json: {e}"))?;
    }

    // Copy roles/ directory
    let roles_src = defaults.join("roles");
    let roles_dst = loom_path.join("roles");
    if roles_src.exists() {
        copy_dir_recursive(&roles_src, &roles_dst)
            .map_err(|e| format!("Failed to copy roles directory: {e}"))?;
    }

    // Copy scripts/ directory
    let scripts_src = defaults.join("scripts");
    let scripts_dst = loom_path.join("scripts");
    if scripts_src.exists() {
        copy_dir_recursive(&scripts_src, &scripts_dst)
            .map_err(|e| format!("Failed to copy scripts directory: {e}"))?;
    }

    // Copy .loom-specific README
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Update .gitignore with Loom ephemeral patterns
    update_gitignore(workspace)?;

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace, &defaults, force)?;

    Ok(())
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

/// Setup repository scaffolding files
///
/// Copies CLAUDE.md, AGENTS.md, .claude/, .codex/, and .github/ to the workspace.
/// - Force mode: Overwrites existing files/directories
/// - Non-force mode: Merges directories (copies missing files, keeps existing ones)
/// - Template variables: Substitutes variables in CLAUDE.md, AGENTS.md, and workflow files
///   - `{{REPO_OWNER}}`, `{{REPO_NAME}}`: Repository info from git remote
///   - `{{LOOM_VERSION}}`, `{{LOOM_COMMIT}}`, `{{INSTALL_DATE}}`: Loom installation metadata
fn setup_repository_scaffolding(
    workspace_path: &Path,
    defaults_path: &Path,
    force: bool,
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
        |src: &Path, dst: &Path, name: &str| -> Result<(), String> {
            if src.exists() {
                if force && dst.exists() {
                    fs::remove_file(dst)
                        .map_err(|e| format!("Failed to remove existing {name}: {e}"))?;
                }
                if force || !dst.exists() {
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
                }
            }
            Ok(())
        };

    // Helper to copy directory with force logic
    let copy_directory = |src: &Path, dst: &Path, name: &str| -> Result<(), String> {
        if src.exists() {
            if force && dst.exists() {
                fs::remove_dir_all(dst)
                    .map_err(|e| format!("Failed to remove existing {name}: {e}"))?;
            }
            if force || !dst.exists() {
                copy_dir_recursive(src, dst).map_err(|e| format!("Failed to copy {name}: {e}"))?;
            } else {
                // Merge mode: copy files that don't exist
                merge_dir_recursive(src, dst)
                    .map_err(|e| format!("Failed to merge {name}: {e}"))?;
            }
        }
        Ok(())
    };

    // Copy target-repo-specific CLAUDE.md and AGENTS.md from defaults/.loom/
    // (NOT defaults/CLAUDE.md which is for Loom repo itself)
    // These files contain template variables that need to be substituted
    copy_file_with_substitution(
        &defaults_path.join(".loom").join("CLAUDE.md"),
        &workspace_path.join("CLAUDE.md"),
        "CLAUDE.md",
    )?;

    copy_file_with_substitution(
        &defaults_path.join(".loom").join("AGENTS.md"),
        &workspace_path.join("AGENTS.md"),
        "AGENTS.md",
    )?;

    // Copy .claude/ directory
    copy_directory(
        &defaults_path.join(".claude"),
        &workspace_path.join(".claude"),
        ".claude directory",
    )?;

    // Copy .codex/ directory
    copy_directory(
        &defaults_path.join(".codex"),
        &workspace_path.join(".codex"),
        ".codex directory",
    )?;

    // Copy .github/ directory
    copy_directory(
        &defaults_path.join(".github"),
        &workspace_path.join(".github"),
        ".github directory",
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
        let content = r#"
**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}
**Repository**: {{REPO_OWNER}}/{{REPO_NAME}}
"#;

        // Test with all values provided
        let metadata = LoomMetadata {
            version: Some("1.2.3".to_string()),
            commit: Some("abc1234".to_string()),
            install_date: "2024-01-15".to_string(),
        };

        let result = substitute_template_variables(
            content,
            Some("myorg"),
            Some("myrepo"),
            &metadata,
        );

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

        let result_fallback = substitute_template_variables(
            content,
            None,
            None,
            &metadata_empty,
        );

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

        // Run setup with force=true
        setup_repository_scaffolding(workspace, &defaults, true).unwrap();

        // Verify .claude was overwritten (custom commands removed)
        assert!(!workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());

        // Verify loom.md was overwritten with new content
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md exists
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

        // Run setup with force=false (merge mode)
        setup_repository_scaffolding(workspace, &defaults, false).unwrap();

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

        // Verify loom.md was NOT overwritten (preserved)
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "custom loom command");

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
        setup_repository_scaffolding(workspace, &defaults, false).unwrap();

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
        setup_repository_scaffolding(workspace, &defaults, true).unwrap();

        // Verify package.json was NOT overwritten
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("my-rust-project"));
        assert!(!content.contains("loom-workspace"));
    }
}
