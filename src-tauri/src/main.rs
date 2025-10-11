// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::Path;
use std::fs;
use std::io;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! Welcome to Loom.", name)
}

#[tauri::command]
fn validate_git_repo(path: String) -> Result<bool, String> {
    let workspace_path = Path::new(&path);

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
fn check_loom_initialized(path: String) -> Result<bool, String> {
    let workspace_path = Path::new(&path);
    let loom_path = workspace_path.join(".loom");

    Ok(loom_path.exists())
}

// Helper function to copy directory recursively
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

#[tauri::command]
fn initialize_loom_workspace(path: String, defaults_path: String) -> Result<(), String> {
    let workspace_path = Path::new(&path);
    let loom_path = workspace_path.join(".loom");

    // Check if .loom already exists
    if loom_path.exists() {
        return Err("Workspace already initialized (.loom directory exists)".to_string());
    }

    // In dev mode, the cwd is src-tauri, so look in parent directory
    let defaults = if Path::new(&defaults_path).exists() {
        Path::new(&defaults_path).to_path_buf()
    } else {
        // Try parent directory (for dev mode when cwd is src-tauri)
        Path::new("..").join(&defaults_path)
    };

    if !defaults.exists() {
        return Err(format!("Defaults directory not found at: {} or ../{}", defaults_path, defaults_path));
    }

    copy_dir_recursive(&defaults, &loom_path)
        .map_err(|e| format!("Failed to copy defaults: {}", e))?;

    // Add .loom/ to .gitignore
    let gitignore_path = workspace_path.join(".gitignore");
    let loom_entry = ".loom/\n";

    // Check if .gitignore exists and if .loom/ is already in it
    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {}", e))?;

        if !contents.contains(".loom/") {
            // Append .loom/ to .gitignore
            let mut new_contents = contents;
            if !new_contents.ends_with('\n') {
                new_contents.push('\n');
            }
            new_contents.push_str(loom_entry);

            fs::write(&gitignore_path, new_contents)
                .map_err(|e| format!("Failed to write .gitignore: {}", e))?;
        }
    } else {
        // Create .gitignore with .loom/
        fs::write(&gitignore_path, loom_entry)
            .map_err(|e| format!("Failed to create .gitignore: {}", e))?;
    }

    Ok(())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            greet,
            validate_git_repo,
            check_loom_initialized,
            initialize_loom_workspace
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
