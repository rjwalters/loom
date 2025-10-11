// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::Path;

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

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![greet, validate_git_repo])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
