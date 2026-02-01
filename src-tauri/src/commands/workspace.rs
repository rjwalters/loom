use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// Workspace data structure for storing last opened workspace
#[derive(serde::Serialize, serde::Deserialize)]
struct WorkspaceData {
    last_workspace_path: String,
    last_opened_at: i64,
}

/// Helper function to copy directory recursively
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
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

/// Helper function to selectively copy .loom configuration from defaults
/// Only copies files/directories that belong in .loom/, not workspace root
pub fn copy_loom_config(defaults_path: &Path, loom_path: &Path) -> Result<(), String> {
    fs::create_dir_all(loom_path).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Files/directories that should be copied to .loom/
    // Note: README.md is handled separately via .loom-README.md copy
    let loom_items = ["config.json", "roles", "scripts"];

    for item in &loom_items {
        let src = defaults_path.join(item);
        let dst = loom_path.join(item);

        if !src.exists() {
            continue; // Skip if source doesn't exist
        }

        if src.is_dir() {
            copy_dir_recursive(&src, &dst).map_err(|e| format!("Failed to copy {item}: {e}"))?;
        } else {
            fs::copy(&src, &dst).map_err(|e| format!("Failed to copy {item}: {e}"))?;
        }
    }

    Ok(())
}

/// Helper function to setup repository scaffolding (CLAUDE.md, .claude/, .codex/)
///
/// Copies workspace-root files from defaults/.loom/ (templates for target repos).
/// Note: defaults/CLAUDE.md is for the Loom repo itself (dogfooding).
pub fn setup_repository_scaffolding(
    workspace_path: &Path,
    defaults_path: &Path,
) -> Result<(), String> {
    // Copy target-repo-specific CLAUDE.md from defaults/.loom/
    // (NOT defaults/CLAUDE.md which is for Loom repo itself)
    let claude_md_dst = workspace_path.join("CLAUDE.md");
    if !claude_md_dst.exists() {
        let claude_md_src = defaults_path.join(".loom").join("CLAUDE.md");
        if claude_md_src.exists() {
            fs::copy(&claude_md_src, &claude_md_dst)
                .map_err(|e| format!("Failed to copy CLAUDE.md: {e}"))?;
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

/// Helper function to resolve defaults directory path
/// Tries development path first, then falls back to bundled resource path
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
                // This handles edge cases where the directory structure might differ
                tried_paths.push(resources_dir.display().to_string());
                if resources_dir.join("config.json").exists() {
                    // If config.json exists directly in Resources/, that's our defaults dir
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

/// Helper function to get workspace file path
fn get_workspace_file_path(app_handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data directory: {e}"))?;

    // Ensure app data directory exists
    if !app_data_dir.exists() {
        fs::create_dir_all(&app_data_dir)
            .map_err(|e| format!("Failed to create app data directory: {e}"))?;
    }

    Ok(app_data_dir.join("workspace.json"))
}

#[tauri::command]
pub fn validate_git_repo(path: &str) -> Result<bool, String> {
    let workspace_path = Path::new(path);

    // Check if the path exists
    if !workspace_path.exists() {
        return Err("Path does not exist".to_string());
    }

    // Check if it's a directory
    if !workspace_path.is_dir() {
        return Err("Path is not a directory".to_string());
    }

    // Check for .git directory
    let git_path = workspace_path.join(".git");
    if !git_path.exists() {
        return Err("Not a git repository (no .git directory found)".to_string());
    }

    Ok(true)
}

#[tauri::command]
pub fn check_loom_initialized(path: &str) -> bool {
    let workspace_path = Path::new(path);
    let loom_path = workspace_path.join(".loom");

    loom_path.exists()
}

#[tauri::command]
pub fn ensure_workspace_scaffolding(workspace_path: &str) -> Result<(), String> {
    let workspace = Path::new(workspace_path);

    // Validate that workspace exists and is a directory
    if !workspace.exists() {
        return Err(format!("Workspace path does not exist: {workspace_path}"));
    }

    if !workspace.is_dir() {
        return Err(format!("Workspace path is not a directory: {workspace_path}"));
    }

    // Get defaults path
    let defaults = resolve_defaults_path("defaults")?;

    // Setup scaffolding (only copies files that don't exist)
    setup_repository_scaffolding(workspace, &defaults)?;

    Ok(())
}

#[tauri::command]
pub fn initialize_loom_workspace(path: &str, defaults_path: &str) -> Result<(), String> {
    let workspace_path = Path::new(path);
    let loom_path = workspace_path.join(".loom");

    // Check if .loom already exists
    if loom_path.exists() {
        return Err("Workspace already initialized (.loom directory exists)".to_string());
    }

    // Copy defaults to .loom (only .loom-specific files, not workspace root files)
    let defaults = resolve_defaults_path(defaults_path)?;

    copy_loom_config(&defaults, &loom_path)?;

    // Copy workspace-specific README (overwriting defaults/README.md)
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Add Loom ephemeral files to .gitignore (not the entire .loom/ directory)
    let gitignore_path = workspace_path.join(".gitignore");

    // Ephemeral files that should be ignored
    let ephemeral_patterns = [
        ".loom/state.json",
        ".loom/worktrees/",
        ".loom/*.log",
        ".loom/*.sock",
    ];

    // Check if .gitignore exists
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

    // Setup repository scaffolding (CLAUDE.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace_path, &defaults)?;

    Ok(())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_stored_workspace(app_handle: tauri::AppHandle) -> Result<Option<String>, String> {
    let workspace_file = get_workspace_file_path(&app_handle)?;

    if !workspace_file.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&workspace_file)
        .map_err(|e| format!("Failed to read workspace file: {e}"))?;

    let workspace_data: WorkspaceData = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse workspace file: {e}"))?;

    Ok(Some(workspace_data.last_workspace_path))
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_stored_workspace(app_handle: tauri::AppHandle, path: &str) -> Result<(), String> {
    // Validate path exists and is a git repo
    validate_git_repo(path)?;

    let workspace_file = get_workspace_file_path(&app_handle)?;

    let workspace_data = WorkspaceData {
        last_workspace_path: path.to_string(),
        #[allow(clippy::cast_possible_truncation)]
        last_opened_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("Failed to get current time: {e}"))?
            .as_millis() as i64,
    };

    let json = serde_json::to_string_pretty(&workspace_data)
        .map_err(|e| format!("Failed to serialize workspace data: {e}"))?;

    fs::write(&workspace_file, json).map_err(|e| format!("Failed to write workspace file: {e}"))?;

    Ok(())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn clear_stored_workspace(app_handle: tauri::AppHandle) -> Result<(), String> {
    let workspace_file = get_workspace_file_path(&app_handle)?;

    if workspace_file.exists() {
        fs::remove_file(&workspace_file)
            .map_err(|e| format!("Failed to remove workspace file: {e}"))?;
    }

    Ok(())
}

#[tauri::command]
pub fn reset_workspace_to_defaults(
    workspace_path: &str,
    defaults_path: &str,
) -> Result<(), String> {
    let workspace = Path::new(workspace_path);
    let loom_path = workspace.join(".loom");

    // Delete existing .loom directory
    if loom_path.exists() {
        fs::remove_dir_all(&loom_path).map_err(|e| format!("Failed to delete .loom: {e}"))?;
    }

    // Copy defaults back (only .loom-specific files, not workspace root files)
    let defaults = resolve_defaults_path(defaults_path)?;

    copy_loom_config(&defaults, &loom_path)?;

    // Copy workspace-specific README (overwriting defaults/README.md)
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Add Loom ephemeral files to .gitignore (ensures patterns are present on factory reset)
    let gitignore_path = workspace.join(".gitignore");

    // Ephemeral files that should be ignored
    let ephemeral_patterns = [
        ".loom/state.json",
        ".loom/worktrees/",
        ".loom/*.log",
        ".loom/*.sock",
    ];

    // Check if .gitignore exists
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

    // Setup repository scaffolding (CLAUDE.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace, &defaults)?;

    Ok(())
}
