// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::io;
use std::path::Path;

mod daemon_client;

use daemon_client::{DaemonClient, Request, Response, TerminalInfo};

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! Welcome to Loom.", name)
}

#[tauri::command]
async fn create_terminal(
    name: String,
    working_dir: Option<String>,
) -> Result<String, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::CreateTerminal { name, working_dir })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalCreated { id } => Ok(id),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn list_terminals() -> Result<Vec<TerminalInfo>, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::ListTerminals)
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalList { terminals } => Ok(terminals),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn destroy_terminal(id: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::DestroyTerminal { id })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn send_terminal_input(id: String, data: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::SendInput { id, data })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[derive(serde::Serialize)]
struct TerminalOutput {
    output: String,
    line_count: i32,
}

#[tauri::command]
async fn get_terminal_output(
    id: String,
    start_line: Option<i32>,
) -> Result<TerminalOutput, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::GetTerminalOutput { id, start_line })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalOutput { output, line_count } => Ok(TerminalOutput { output, line_count }),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
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

    // Copy defaults to .loom (symlink in src-tauri/ points to ../defaults/)
    let defaults = Path::new(&defaults_path);
    if !defaults.exists() {
        return Err(format!("Defaults directory not found: {}", defaults_path));
    }

    copy_dir_recursive(defaults, &loom_path)
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

#[tauri::command]
fn read_config(workspace_path: String) -> Result<String, String> {
    let config_path = Path::new(&workspace_path).join(".loom").join("config.json");

    if !config_path.exists() {
        return Err("Config file does not exist".to_string());
    }

    fs::read_to_string(&config_path)
        .map_err(|e| format!("Failed to read config: {}", e))
}

#[tauri::command]
fn write_config(workspace_path: String, config_json: String) -> Result<(), String> {
    let loom_dir = Path::new(&workspace_path).join(".loom");
    let config_path = loom_dir.join("config.json");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        fs::create_dir_all(&loom_dir)
            .map_err(|e| format!("Failed to create .loom directory: {}", e))?;
    }

    fs::write(&config_path, config_json)
        .map_err(|e| format!("Failed to write config: {}", e))
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            greet,
            validate_git_repo,
            check_loom_initialized,
            initialize_loom_workspace,
            read_config,
            write_config,
            create_terminal,
            list_terminals,
            destroy_terminal,
            send_terminal_input,
            get_terminal_output
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
