//! Git detection, validation, and path resolution
//!
//! Functions for detecting repository type, validating git structure,
//! and resolving paths relative to git roots.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ValidationReport;

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
pub fn extract_repo_info(workspace_path: &Path) -> Option<(String, String)> {
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
        let is_loom_repo =
            (workspace_path.join("src-tauri").is_dir() || owner == "rjwalters") && repo == "loom";

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
pub fn validate_loom_source_repo(workspace_path: &Path) -> ValidationReport {
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

/// Validate that a path is a git repository
///
/// Checks that the path exists, is a directory, and contains a .git directory.
pub fn validate_git_repository(path: &str) -> Result<(), String> {
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
pub fn find_git_root() -> Option<PathBuf> {
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
pub fn resolve_defaults_path(defaults_path: &str) -> Result<PathBuf, String> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
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
}
