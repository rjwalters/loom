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

    // Copy defaults to .loom
    copy_dir_recursive(&defaults, &loom_path)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Copy workspace-specific README (overwriting defaults/README.md)
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Update .gitignore with Loom ephemeral patterns
    update_gitignore(workspace)?;

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace, &defaults)?;

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
        if git_dir.exists() {
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

/// Setup repository scaffolding files
///
/// Copies CLAUDE.md, AGENTS.md, .claude/, and .codex/ to the workspace if they don't exist.
/// Only creates files that are missing - never overwrites existing files.
fn setup_repository_scaffolding(workspace_path: &Path, defaults_path: &Path) -> Result<(), String> {
    // Copy CLAUDE.md if it doesn't exist
    let claude_md_dst = workspace_path.join("CLAUDE.md");
    if !claude_md_dst.exists() {
        let claude_md_src = defaults_path.join("CLAUDE.md");
        if claude_md_src.exists() {
            fs::copy(&claude_md_src, &claude_md_dst)
                .map_err(|e| format!("Failed to copy CLAUDE.md: {e}"))?;
        }
    }

    // Copy AGENTS.md if it doesn't exist
    let agents_md_dst = workspace_path.join("AGENTS.md");
    if !agents_md_dst.exists() {
        let agents_md_src = defaults_path.join("AGENTS.md");
        if agents_md_src.exists() {
            fs::copy(&agents_md_src, &agents_md_dst)
                .map_err(|e| format!("Failed to copy AGENTS.md: {e}"))?;
        }
    }

    // Copy .claude/ directory if it doesn't exist
    let claude_dir_dst = workspace_path.join(".claude");
    if !claude_dir_dst.exists() {
        let claude_dir_src = defaults_path.join(".claude");
        if claude_dir_src.exists() {
            copy_dir_recursive(&claude_dir_src, &claude_dir_dst)
                .map_err(|e| format!("Failed to copy .claude directory: {e}"))?;
        }
    }

    // Copy .codex/ directory if it doesn't exist
    let codex_dir_dst = workspace_path.join(".codex");
    if !codex_dir_dst.exists() {
        let codex_dir_src = defaults_path.join(".codex");
        if codex_dir_src.exists() {
            copy_dir_recursive(&codex_dir_src, &codex_dir_dst)
                .map_err(|e| format!("Failed to copy .codex directory: {e}"))?;
        }
    }

    // Copy .github/ directory if it doesn't exist
    let github_dir_dst = workspace_path.join(".github");
    if !github_dir_dst.exists() {
        let github_dir_src = defaults_path.join(".github");
        if github_dir_src.exists() {
            copy_dir_recursive(&github_dir_src, &github_dir_dst)
                .map_err(|e| format!("Failed to copy .github directory: {e}"))?;
        }
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
}
