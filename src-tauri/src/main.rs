// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::Datelike;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Emitter, Manager,
};

mod daemon_client;
mod daemon_manager;
mod dependency_checker;
mod mcp_watcher;

use daemon_client::{DaemonClient, Request, Response, TerminalInfo};

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! Welcome to Loom.")
}

#[tauri::command]
async fn create_terminal(
    config_id: String,
    name: String,
    working_dir: Option<String>,
    role: Option<String>,
    instance_number: Option<u32>,
) -> Result<String, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::CreateTerminal {
            config_id,
            name,
            working_dir,
            role,
            instance_number,
        })
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
    byte_count: usize,
}

#[tauri::command]
async fn get_terminal_output(
    id: String,
    start_byte: Option<usize>,
) -> Result<TerminalOutput, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::GetTerminalOutput { id, start_byte })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalOutput { output, byte_count } => {
            Ok(TerminalOutput { output, byte_count })
        }
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn resize_terminal(id: String, cols: u16, rows: u16) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::ResizeTerminal { id, cols, rows })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn check_session_health(id: String) -> Result<bool, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::CheckSessionHealth { id })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::SessionHealth { has_session } => Ok(has_session),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn check_daemon_health() -> Result<bool, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::Ping)
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Pong => Ok(true),
        _ => Ok(false),
    }
}

#[derive(serde::Serialize)]
struct DaemonStatus {
    running: bool,
    socket_path: String,
    error: Option<String>,
}

#[tauri::command]
async fn get_daemon_status() -> DaemonStatus {
    let socket_path = dirs::home_dir()
        .map(|h| h.join(".loom/loom-daemon.sock"))
        .map_or_else(|| "Unknown".to_string(), |p| p.display().to_string());

    match DaemonClient::new() {
        Ok(client) => match client.send_request(Request::Ping).await {
            Ok(Response::Pong) => DaemonStatus {
                running: true,
                socket_path,
                error: None,
            },
            Ok(_) => DaemonStatus {
                running: false,
                socket_path,
                error: Some("Daemon responded with unexpected response".to_string()),
            },
            Err(e) => DaemonStatus {
                running: false,
                socket_path,
                error: Some(format!("Failed to ping daemon: {e}")),
            },
        },
        Err(e) => DaemonStatus {
            running: false,
            socket_path,
            error: Some(format!("Failed to create client: {e}")),
        },
    }
}

#[tauri::command]
async fn list_available_sessions() -> Result<Vec<String>, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::ListAvailableSessions)
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::AvailableSessions { sessions } => Ok(sessions),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn attach_to_session(id: String, session_name: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::AttachToSession { id, session_name })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn kill_session(session_name: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::KillSession { session_name })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
async fn set_worktree_path(id: String, worktree_path: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::SetWorktreePath { id, worktree_path })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
fn validate_git_repo(path: &str) -> Result<bool, String> {
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
fn check_loom_initialized(path: &str) -> bool {
    let workspace_path = Path::new(path);
    let loom_path = workspace_path.join(".loom");

    loom_path.exists()
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

// Helper function to find git repository root by searching for .git directory
fn find_git_root() -> Option<std::path::PathBuf> {
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

// Helper function to resolve defaults directory path
// Tries development path first, then falls back to bundled resource path
fn resolve_defaults_path(defaults_path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::PathBuf;

    // Try the provided path first (development mode - relative to cwd)
    let dev_path = PathBuf::from(defaults_path);
    if dev_path.exists() {
        return Ok(dev_path);
    }

    // Try finding defaults relative to git repository root
    // This handles the case where we're running from a git worktree
    if let Some(git_root) = find_git_root() {
        let git_root_defaults = git_root.join(defaults_path);
        if git_root_defaults.exists() {
            return Ok(git_root_defaults);
        }
    }

    // Try resolving as bundled resource (production mode)
    // In production, resources are in .app/Contents/Resources/
    if let Ok(exe_path) = std::env::current_exe() {
        // Get the app bundle Resources directory
        if let Some(exe_dir) = exe_path.parent() {
            // exe is in Contents/MacOS/, resources are in Contents/Resources/
            if let Some(contents_dir) = exe_dir.parent() {
                let resources_path = contents_dir.join("Resources").join(defaults_path);
                if resources_path.exists() {
                    return Ok(resources_path);
                }
            }
        }
    }

    Err(format!(
        "Defaults directory not found: tried {defaults_path}, git root, and bundled resources"
    ))
}

#[tauri::command]
fn initialize_loom_workspace(path: &str, defaults_path: &str) -> Result<(), String> {
    let workspace_path = Path::new(path);
    let loom_path = workspace_path.join(".loom");

    // Check if .loom already exists
    if loom_path.exists() {
        return Err("Workspace already initialized (.loom directory exists)".to_string());
    }

    // Copy defaults to .loom
    let defaults = resolve_defaults_path(defaults_path)?;

    copy_dir_recursive(&defaults, &loom_path)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Copy workspace-specific README (overwriting defaults/README.md)
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Add .loom/ and .loom/worktrees/ to .gitignore
    let gitignore_path = workspace_path.join(".gitignore");

    // Check if .gitignore exists
    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

        let mut new_contents = contents.clone();
        let mut modified = false;

        // Add .loom/ if not present
        if !contents.contains(".loom/") {
            if !new_contents.ends_with('\n') {
                new_contents.push('\n');
            }
            new_contents.push_str(".loom/\n");
            modified = true;
        }

        // Add .loom/worktrees/ if not present
        if !contents.contains(".loom/worktrees/") {
            if !new_contents.ends_with('\n') {
                new_contents.push('\n');
            }
            new_contents.push_str(".loom/worktrees/\n");
            modified = true;
        }

        // Write back if we made changes
        if modified {
            fs::write(&gitignore_path, new_contents)
                .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
        }
    } else {
        // Create .gitignore with both entries
        let loom_entries = ".loom/\n.loom/worktrees/\n";
        fs::write(&gitignore_path, loom_entries)
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
    }

    Ok(())
}

#[tauri::command]
fn read_config(workspace_path: &str) -> Result<String, String> {
    let config_path = Path::new(workspace_path).join(".loom").join("config.json");

    if !config_path.exists() {
        return Err("Config file does not exist".to_string());
    }

    fs::read_to_string(&config_path).map_err(|e| format!("Failed to read config: {e}"))
}

#[tauri::command]
fn write_config(workspace_path: &str, config_json: String) -> Result<(), String> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let config_path = loom_dir.join("config.json");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        fs::create_dir_all(&loom_dir)
            .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
    }

    fs::write(&config_path, config_json).map_err(|e| format!("Failed to write config: {e}"))
}

#[tauri::command]
fn read_state(workspace_path: &str) -> Result<String, String> {
    let state_path = Path::new(workspace_path).join(".loom").join("state.json");

    if !state_path.exists() {
        return Err("State file does not exist".to_string());
    }

    fs::read_to_string(&state_path).map_err(|e| format!("Failed to read state: {e}"))
}

#[tauri::command]
fn write_state(workspace_path: &str, state_json: String) -> Result<(), String> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let state_path = loom_dir.join("state.json");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        fs::create_dir_all(&loom_dir)
            .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
    }

    fs::write(&state_path, state_json).map_err(|e| format!("Failed to write state: {e}"))
}

#[tauri::command]
fn list_role_files(workspace_path: &str) -> Result<Vec<String>, String> {
    let roles_dir = Path::new(workspace_path).join(".loom").join("roles");

    if !roles_dir.exists() {
        return Ok(Vec::new());
    }

    let mut role_files = Vec::new();

    let entries =
        fs::read_dir(&roles_dir).map_err(|e| format!("Failed to read roles directory: {e}"))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read directory entry: {e}"))?;
        let path = entry.path();

        if path.is_file() {
            if let Some(extension) = path.extension() {
                if extension == "md" {
                    if let Some(filename) = path.file_name() {
                        if let Some(name) = filename.to_str() {
                            // Filter out README.md - it's documentation, not a role
                            if name != "README.md" {
                                role_files.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    role_files.sort();
    Ok(role_files)
}

#[tauri::command]
fn read_role_file(workspace_path: &str, filename: &str) -> Result<String, String> {
    let role_path = Path::new(workspace_path)
        .join(".loom")
        .join("roles")
        .join(filename);

    if !role_path.exists() {
        return Err("Role file does not exist".to_string());
    }

    fs::read_to_string(&role_path).map_err(|e| format!("Failed to read role file: {e}"))
}

#[tauri::command]
fn read_role_metadata(workspace_path: &str, filename: &str) -> Result<Option<String>, String> {
    // Convert .md filename to .json filename
    let json_filename = if let Some(stem) = filename.strip_suffix(".md") {
        format!("{stem}.json")
    } else {
        return Ok(None);
    };

    let metadata_path = Path::new(workspace_path)
        .join(".loom")
        .join("roles")
        .join(&json_filename);

    if !metadata_path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(&metadata_path).map_err(|e| format!("Failed to read metadata: {e}"))?;
    Ok(Some(content))
}

#[tauri::command]
fn get_env_var(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("{key} not set in .env"))
}

#[tauri::command]
fn check_claude_code() -> Result<bool, String> {
    Command::new("which")
        .arg("claude-code")
        .output()
        .map(|o| o.status.success())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn read_text_file(path: &str) -> Result<String, String> {
    let file_path = Path::new(path);

    // Check if file exists
    if !file_path.exists() {
        return Err(format!("File does not exist: {path}"));
    }

    // Check if it's a file (not a directory)
    if !file_path.is_file() {
        return Err(format!("Path is not a file: {path}"));
    }

    fs::read_to_string(file_path).map_err(|e| format!("Failed to read file: {e}"))
}

#[tauri::command]
fn write_file(path: &str, content: &str) -> Result<(), String> {
    let file_path = Path::new(path);

    // Ensure parent directory exists
    if let Some(parent) = file_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create parent directory: {e}"))?;
        }
    }

    fs::write(file_path, content).map_err(|e| format!("Failed to write file: {e}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WorkspaceData {
    last_workspace_path: String,
    last_opened_at: i64,
}

fn get_workspace_file_path(app_handle: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
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
#[allow(clippy::needless_pass_by_value)]
fn get_stored_workspace(app_handle: tauri::AppHandle) -> Result<Option<String>, String> {
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
fn set_stored_workspace(app_handle: tauri::AppHandle, path: &str) -> Result<(), String> {
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
fn clear_stored_workspace(app_handle: tauri::AppHandle) -> Result<(), String> {
    let workspace_file = get_workspace_file_path(&app_handle)?;

    if workspace_file.exists() {
        fs::remove_file(&workspace_file)
            .map_err(|e| format!("Failed to remove workspace file: {e}"))?;
    }

    Ok(())
}

#[tauri::command]
fn reset_workspace_to_defaults(workspace_path: &str, defaults_path: &str) -> Result<(), String> {
    let workspace = Path::new(workspace_path);
    let loom_path = workspace.join(".loom");

    // Delete existing .loom directory
    if loom_path.exists() {
        fs::remove_dir_all(&loom_path).map_err(|e| format!("Failed to delete .loom: {e}"))?;
    }

    // Copy defaults back
    let defaults = resolve_defaults_path(defaults_path)?;

    copy_dir_recursive(&defaults, &loom_path)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Copy workspace-specific README (overwriting defaults/README.md)
    let loom_readme_src = defaults.join(".loom-README.md");
    let loom_readme_dst = loom_path.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Add .loom/ and .loom/worktrees/ to .gitignore (ensures both entries are present on factory reset)
    let gitignore_path = workspace.join(".gitignore");

    // Check if .gitignore exists
    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

        let mut new_contents = contents.clone();
        let mut modified = false;

        // Add .loom/ if not present
        if !contents.contains(".loom/") {
            if !new_contents.ends_with('\n') {
                new_contents.push('\n');
            }
            new_contents.push_str(".loom/\n");
            modified = true;
        }

        // Add .loom/worktrees/ if not present
        if !contents.contains(".loom/worktrees/") {
            if !new_contents.ends_with('\n') {
                new_contents.push('\n');
            }
            new_contents.push_str(".loom/worktrees/\n");
            modified = true;
        }

        // Write back if we made changes
        if modified {
            fs::write(&gitignore_path, new_contents)
                .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
        }
    } else {
        // Create .gitignore with both entries
        let loom_entries = ".loom/\n.loom/worktrees/\n";
        fs::write(&gitignore_path, loom_entries)
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
    }

    Ok(())
}

/// Check if the current directory has a GitHub remote
#[tauri::command]
fn check_github_remote() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .output()
        .map_err(|e| format!("Failed to run git remote: {e}"))?;

    if !output.status.success() {
        return Ok(false);
    }

    let remotes = String::from_utf8_lossy(&output.stdout);
    Ok(remotes.contains("github.com"))
}

/// Check if a GitHub label exists
#[tauri::command]
fn check_label_exists(name: &str) -> Result<bool, String> {
    let output = Command::new("gh")
        .args([
            "label",
            "list",
            "--json",
            "name",
            "--jq",
            &format!(".[].name | select(. == \"{name}\")"),
        ])
        .output()
        .map_err(|e| format!("Failed to run gh label list: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label list failed: {stderr}"));
    }

    let result = String::from_utf8_lossy(&output.stdout);
    Ok(!result.trim().is_empty())
}

/// Create a GitHub label
#[tauri::command]
fn create_github_label(name: &str, description: &str, color: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args([
            "label",
            "create",
            name,
            "--description",
            description,
            "--color",
            color,
        ])
        .output()
        .map_err(|e| format!("Failed to run gh label create: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label create failed: {stderr}"));
    }

    Ok(())
}

/// Update a GitHub label (requires --force flag equivalent)
#[tauri::command]
fn update_github_label(name: &str, description: &str, color: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args([
            "label",
            "create",
            name,
            "--description",
            description,
            "--color",
            color,
            "--force",
        ])
        .output()
        .map_err(|e| format!("Failed to run gh label create --force: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label update failed: {stderr}"));
    }

    Ok(())
}

#[derive(serde::Serialize)]
struct LabelResetResult {
    issues_cleaned: usize,
    prs_updated: usize,
    errors: Vec<String>,
}

/// Reset GitHub label state machine by cleaning up in-progress labels
#[tauri::command]
fn reset_github_labels() -> Result<LabelResetResult, String> {
    let mut result = LabelResetResult {
        issues_cleaned: 0,
        prs_updated: 0,
        errors: Vec::new(),
    };

    // Step 1: Remove loom:in-progress from all open issues
    let issues_output = Command::new("gh")
        .args([
            "issue",
            "list",
            "--label",
            "loom:in-progress",
            "--state",
            "open",
            "--json",
            "number",
            "--jq",
            ".[].number",
        ])
        .output()
        .map_err(|e| format!("Failed to list issues: {e}"))?;

    if issues_output.status.success() {
        let issue_numbers = String::from_utf8_lossy(&issues_output.stdout);
        for issue_num in issue_numbers.lines() {
            if issue_num.trim().is_empty() {
                continue;
            }

            let remove_output = Command::new("gh")
                .args([
                    "issue",
                    "edit",
                    issue_num,
                    "--remove-label",
                    "loom:in-progress",
                ])
                .output()
                .map_err(|e| format!("Failed to remove label: {e}"))?;

            if remove_output.status.success() {
                result.issues_cleaned += 1;
            } else {
                let error = format!(
                    "Failed to remove loom:in-progress from issue {issue_num}: {}",
                    String::from_utf8_lossy(&remove_output.stderr)
                );
                result.errors.push(error);
            }
        }
    }

    // Step 2: Replace loom:reviewing with loom:review-requested on PRs
    let prs_output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--label",
            "loom:reviewing",
            "--state",
            "open",
            "--json",
            "number",
            "--jq",
            ".[].number",
        ])
        .output()
        .map_err(|e| format!("Failed to list PRs: {e}"))?;

    if prs_output.status.success() {
        let pr_numbers = String::from_utf8_lossy(&prs_output.stdout);
        for pr_num in pr_numbers.lines() {
            if pr_num.trim().is_empty() {
                continue;
            }

            let edit_output = Command::new("gh")
                .args([
                    "pr",
                    "edit",
                    pr_num,
                    "--remove-label",
                    "loom:reviewing",
                    "--add-label",
                    "loom:review-requested",
                ])
                .output()
                .map_err(|e| format!("Failed to update PR labels: {e}"))?;

            if edit_output.status.success() {
                result.prs_updated += 1;
            } else {
                let error = format!(
                    "Failed to update labels on PR {pr_num}: {}",
                    String::from_utf8_lossy(&edit_output.stderr)
                );
                result.errors.push(error);
            }
        }
    }

    Ok(result)
}

/// Create a new local git repository with Loom configuration
#[tauri::command]
fn create_local_project(
    name: &str,
    location: &str,
    description: Option<String>,
    license: Option<String>,
) -> Result<String, String> {
    let project_path = Path::new(location).join(name);

    // Check if directory already exists
    if project_path.exists() {
        return Err(format!("Directory already exists: {}", project_path.display()));
    }

    // Create project directory
    fs::create_dir_all(&project_path)
        .map_err(|e| format!("Failed to create project directory: {e}"))?;

    // Initialize git repository
    let init_output = Command::new("git")
        .args(["init"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git init: {e}"))?;

    if !init_output.status.success() {
        let stderr = String::from_utf8_lossy(&init_output.stderr);
        return Err(format!("git init failed: {stderr}"));
    }

    // Create README.md
    let readme_content = if let Some(desc) = description {
        format!("# {name}\n\n{desc}\n")
    } else {
        format!("# {name}\n")
    };

    fs::write(project_path.join("README.md"), readme_content)
        .map_err(|e| format!("Failed to create README.md: {e}"))?;

    // Create LICENSE file if specified
    if let Some(license_type) = license {
        let license_content = generate_license_content(&license_type, name)?;
        fs::write(project_path.join("LICENSE"), license_content)
            .map_err(|e| format!("Failed to create LICENSE: {e}"))?;
    }

    // Initialize .loom directory with defaults
    init_loom_directory(&project_path)?;

    // Create initial .gitignore with .loom/ and .loom/worktrees/
    let gitignore_content = ".loom/\n.loom/worktrees/\n";
    fs::write(project_path.join(".gitignore"), gitignore_content)
        .map_err(|e| format!("Failed to create .gitignore: {e}"))?;

    // Create initial git commit
    let add_output = Command::new("git")
        .args(["add", "."])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git add: {e}"))?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        return Err(format!("git add failed: {stderr}"));
    }

    let commit_output = Command::new("git")
        .args(["commit", "-m", "Initial commit"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git commit: {e}"))?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        return Err(format!("git commit failed: {stderr}"));
    }

    Ok(project_path.to_string_lossy().to_string())
}

/// Create a GitHub repository and push the local project
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn create_github_repository(
    project_path: &str,
    name: &str,
    description: Option<String>,
    is_private: bool,
) -> Result<String, String> {
    let project = Path::new(project_path);

    // Check if project directory exists
    if !project.exists() {
        return Err(format!("Project directory does not exist: {project_path}"));
    }

    // Check if gh CLI is available
    let which_output = Command::new("which")
        .arg("gh")
        .output()
        .map_err(|e| format!("Failed to check for gh CLI: {e}"))?;

    if !which_output.status.success() {
        return Err(
            "GitHub CLI (gh) is not installed. Please install it from https://cli.github.com/"
                .to_string(),
        );
    }

    // Check if gh is authenticated
    let auth_status = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map_err(|e| format!("Failed to check gh auth status: {e}"))?;

    if !auth_status.status.success() {
        return Err(
            "Not authenticated with GitHub CLI. Please run 'gh auth login' first.".to_string()
        );
    }

    // Build gh repo create command
    let mut args = vec!["repo", "create", name];

    // Add description if provided
    let desc_arg;
    if let Some(ref desc) = description {
        desc_arg = format!("--description={desc}");
        args.push(&desc_arg);
    }

    // Add visibility flag
    if is_private {
        args.push("--private");
    } else {
        args.push("--public");
    }

    // Add source flag to push existing local repo
    args.push("--source=.");
    args.push("--remote=origin");
    args.push("--push");

    // Create GitHub repository
    let create_output = Command::new("gh")
        .args(&args)
        .current_dir(project)
        .output()
        .map_err(|e| format!("Failed to run gh repo create: {e}"))?;

    if !create_output.status.success() {
        let stderr = String::from_utf8_lossy(&create_output.stderr);
        return Err(format!("GitHub repository creation failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&create_output.stdout);
    Ok(stdout.to_string())
}

/// Initialize .loom directory with default configuration
fn init_loom_directory(project_path: &Path) -> Result<(), String> {
    let loom_dir = project_path.join(".loom");

    // Create .loom directory
    fs::create_dir_all(&loom_dir).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy default config from defaults directory
    let defaults_dir = resolve_defaults_path("defaults")?;

    // Copy entire defaults directory structure to .loom
    copy_dir_recursive(&defaults_dir, &loom_dir)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Copy .loom-README.md to .loom/README.md if it exists
    let loom_readme_src = defaults_dir.join(".loom-README.md");
    let loom_readme_dst = loom_dir.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    Ok(())
}

/// Generate license content based on license type
#[tauri::command]
fn check_system_dependencies() -> dependency_checker::DependencyStatus {
    dependency_checker::check_dependencies()
}

fn generate_license_content(license_type: &str, project_name: &str) -> Result<String, String> {
    let year = chrono::Local::now().year();

    match license_type {
        "MIT" => Ok(format!(
            r#"MIT License

Copyright (c) {year} {project_name}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"#
        )),
        "Apache-2.0" => Ok(format!(
            r#"                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   Copyright {year} {project_name}

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
"#
        )),
        _ => Err(format!("Unsupported license type: {license_type}")),
    }
}

/// Emit a menu event programmatically (for MCP control)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn emit_menu_event(window: tauri::Window, event_name: &str) -> Result<(), String> {
    window
        .emit(event_name, ())
        .map_err(|e| format!("Failed to emit event: {e}"))
}

/// Emit any event programmatically (for MCP file watcher)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn emit_event(window: tauri::Window, event: &str) -> Result<(), String> {
    window
        .emit(event, ())
        .map_err(|e| format!("Failed to emit event: {e}"))
}

/// Append console log message to file for MCP access
#[tauri::command]
fn append_to_console_log(content: &str) -> Result<(), String> {
    use std::io::Write;

    let log_path = dirs::home_dir()
        .ok_or_else(|| "Could not get home directory".to_string())?
        .join(".loom")
        .join("console.log");

    // Ensure .loom directory exists
    if let Some(parent) = log_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
        }
    }

    // Open file in append mode (create if doesn't exist)
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("Failed to open log file: {e}"))?;

    // Write content
    file.write_all(content.as_bytes())
        .map_err(|e| format!("Failed to write to log: {e}"))?;

    Ok(())
}

/// Trigger workspace start programmatically (for MCP/testing)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn trigger_start(window: tauri::Window) -> Result<(), String> {
    window
        .emit("start-workspace", ())
        .map_err(|e| format!("Failed to emit start-workspace event: {e}"))
}

/// Trigger force start programmatically (for MCP/testing)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn trigger_force_start(window: tauri::Window) -> Result<(), String> {
    window
        .emit("force-start-workspace", ())
        .map_err(|e| format!("Failed to emit force-start-workspace event: {e}"))
}

/// Trigger factory reset programmatically (for MCP/testing)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn trigger_factory_reset(window: tauri::Window) -> Result<(), String> {
    window
        .emit("factory-reset-workspace", ())
        .map_err(|e| format!("Failed to emit factory-reset-workspace event: {e}"))
}

/// Kill all loom tmux sessions
#[tauri::command]
fn kill_all_loom_sessions() -> Result<(), String> {
    eprintln!("[kill_all_loom_sessions] Killing all loom tmux sessions");

    let output = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(|e| format!("Failed to list tmux sessions: {e}"))?;

    if !output.status.success() {
        // tmux list-sessions fails if no sessions exist - this is OK
        eprintln!("[kill_all_loom_sessions] No tmux sessions found");
        return Ok(());
    }

    let sessions = String::from_utf8_lossy(&output.stdout);
    let mut killed_count = 0;

    for session in sessions.lines() {
        if session.starts_with("loom-") {
            eprintln!("[kill_all_loom_sessions] Killing tmux session: {session}");

            let kill_output = Command::new("tmux")
                .args(["-L", "loom", "kill-session", "-t", session])
                .output()
                .map_err(|e| format!("Failed to kill session {session}: {e}"))?;

            if kill_output.status.success() {
                killed_count += 1;
            } else {
                eprintln!(
                    "[kill_all_loom_sessions] Failed to kill {session}: {}",
                    String::from_utf8_lossy(&kill_output.stderr)
                );
            }
        }
    }

    eprintln!("[kill_all_loom_sessions] Killed {killed_count} sessions");
    Ok(())
}

fn build_menu<R: tauri::Runtime>(
    handle: &impl tauri::Manager<R>,
) -> Result<tauri::menu::Menu<R>, tauri::Error> {
    // Build File menu
    let new_terminal = MenuItemBuilder::new("New Terminal")
        .id("new_terminal")
        .accelerator("CmdOrCtrl+T")
        .build(handle)?;
    let close_terminal = MenuItemBuilder::new("Close Terminal")
        .id("close_terminal")
        .accelerator("CmdOrCtrl+Shift+W")
        .build(handle)?;
    let close_workspace = MenuItemBuilder::new("Close Workspace")
        .id("close_workspace")
        .accelerator("CmdOrCtrl+W")
        .build(handle)?;
    let start_workspace = MenuItemBuilder::new("Start...")
        .id("start_workspace")
        .accelerator("CmdOrCtrl+Shift+R")
        .build(handle)?;
    let force_start_workspace = MenuItemBuilder::new("Force Start")
        .id("force_start_workspace")
        .accelerator("CmdOrCtrl+Shift+Alt+R")
        .build(handle)?;
    let factory_reset_workspace = MenuItemBuilder::new("Factory Reset...")
        .id("factory_reset_workspace")
        .build(handle)?;

    let file_menu = SubmenuBuilder::new(handle, "File")
        .item(&new_terminal)
        .item(&close_terminal)
        .separator()
        .item(&close_workspace)
        .item(&start_workspace)
        .item(&force_start_workspace)
        .item(&factory_reset_workspace)
        .separator()
        .quit()
        .build()?;

    // Build Edit menu
    let edit_menu = SubmenuBuilder::new(handle, "Edit")
        .cut()
        .copy()
        .paste()
        .separator()
        .select_all()
        .build()?;

    // Build View menu
    let toggle_theme = MenuItemBuilder::new("Toggle Theme")
        .id("toggle_theme")
        .accelerator("CmdOrCtrl+Shift+T")
        .build(handle)?;
    let zoom_in = MenuItemBuilder::new("Zoom In")
        .id("zoom_in")
        .accelerator("CmdOrCtrl+=")
        .build(handle)?;
    let zoom_out = MenuItemBuilder::new("Zoom Out")
        .id("zoom_out")
        .accelerator("CmdOrCtrl+-")
        .build(handle)?;
    let reset_zoom = MenuItemBuilder::new("Reset Zoom")
        .id("reset_zoom")
        .accelerator("CmdOrCtrl+0")
        .build(handle)?;

    let view_menu = SubmenuBuilder::new(handle, "View")
        .item(&toggle_theme)
        .separator()
        .item(&zoom_in)
        .item(&zoom_out)
        .item(&reset_zoom)
        .separator()
        .fullscreen()
        .build()?;

    // Build Window menu
    let window_menu = SubmenuBuilder::new(handle, "Window")
        .minimize()
        .maximize()
        .build()?;

    // Build Help menu
    let documentation = MenuItemBuilder::new("Documentation")
        .id("documentation")
        .build(handle)?;
    let view_github = MenuItemBuilder::new("View on GitHub")
        .id("view_github")
        .build(handle)?;
    let report_issue = MenuItemBuilder::new("Report Issue")
        .id("report_issue")
        .build(handle)?;
    let daemon_status = MenuItemBuilder::new("Daemon Status...")
        .id("daemon_status")
        .build(handle)?;
    let keyboard_shortcuts = MenuItemBuilder::new("Keyboard Shortcuts")
        .id("keyboard_shortcuts")
        .accelerator("CmdOrCtrl+/")
        .build(handle)?;

    let help_menu = SubmenuBuilder::new(handle, "Help")
        .item(&documentation)
        .item(&view_github)
        .item(&report_issue)
        .separator()
        .item(&daemon_status)
        .item(&keyboard_shortcuts)
        .build()?;

    MenuBuilder::new(handle)
        .item(&file_menu)
        .item(&edit_menu)
        .item(&view_menu)
        .item(&window_menu)
        .item(&help_menu)
        .build()
}

fn handle_menu_event<R: tauri::Runtime>(app: &tauri::AppHandle<R>, event: tauri::menu::MenuEvent) {
    let menu_id = event.id().as_ref();

    match menu_id {
        "new_terminal" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("new-terminal", ());
            }
        }
        "close_terminal" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("close-terminal", ());
            }
        }
        "close_workspace" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("close-workspace", ());
            }
        }
        "start_workspace" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("start-workspace", ());
            }
        }
        "force_start_workspace" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("force-start-workspace", ());
            }
        }
        "factory_reset_workspace" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("factory-reset-workspace", ());
            }
        }
        "toggle_theme" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("toggle-theme", ());
            }
        }
        "zoom_in" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("zoom-in", ());
            }
        }
        "zoom_out" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("zoom-out", ());
            }
        }
        "reset_zoom" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("reset-zoom", ());
            }
        }
        "documentation" => {
            let _ = tauri_plugin_opener::OpenerExt::opener(app)
                .open_url("https://github.com/rjwalters/loom#readme", None::<&str>);
        }
        "view_github" => {
            let _ = tauri_plugin_opener::OpenerExt::opener(app)
                .open_url("https://github.com/rjwalters/loom", None::<&str>);
        }
        "report_issue" => {
            let _ = tauri_plugin_opener::OpenerExt::opener(app)
                .open_url("https://github.com/rjwalters/loom/issues/new", None::<&str>);
        }
        "keyboard_shortcuts" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-shortcuts", ());
            }
        }
        "daemon_status" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-daemon-status", ());
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    if let Err(e) = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_cli::init())
        .setup(|app| {
            // Check if we're in development or production mode
            let is_production = !cfg!(debug_assertions);

            eprintln!(
                "[Loom] Starting in {} mode",
                if is_production {
                    "production"
                } else {
                    "development"
                }
            );

            // Build and set menu
            let menu = build_menu(app).map_err(|e| format!("Failed to build menu: {e}"))?;
            app.set_menu(menu)
                .map_err(|e| format!("Failed to set menu: {e}"))?;

            // Register menu event handler
            app.on_menu_event(handle_menu_event);

            // Handle CLI arguments
            match tauri_plugin_cli::CliExt::cli(app).matches() {
                Ok(matches) => {
                    // Check for --workspace argument
                    if let Some(workspace_arg) = matches.args.get("workspace") {
                        if let Some(workspace_path) = workspace_arg.value.as_str() {
                            eprintln!("[Loom] CLI workspace argument: {workspace_path}");

                            // Get the main window
                            if let Some(window) = app.get_webview_window("main") {
                                // Emit event to frontend with workspace path
                                window.emit("cli-workspace", workspace_path).map_err(|e| {
                                    format!("Failed to emit cli-workspace event: {e}")
                                })?;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[Loom] Warning: Failed to parse CLI arguments: {e}");
                }
            }

            // Initialize daemon manager
            let mut daemon_manager = daemon_manager::DaemonManager::new()
                .map_err(|e| format!("Failed to create daemon manager: {e}"))?;

            // Ensure daemon is running
            tauri::async_runtime::block_on(async {
                daemon_manager.ensure_daemon_running(is_production).await
            })
            .map_err(|e| format!("Failed to start/connect to daemon: {e}"))?;

            // Store daemon manager in app state for cleanup on quit
            app.manage(std::sync::Mutex::new(daemon_manager));

            // Start MCP command file watcher
            let window = app
                .get_webview_window("main")
                .ok_or_else(|| "Failed to get main window".to_string())?;
            mcp_watcher::start_mcp_watcher(window);
            eprintln!("[Loom] MCP command watcher started");

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // App is quitting - clean up resources
                eprintln!("[Loom] App closing - cleaning up...");

                let is_production = !cfg!(debug_assertions);

                // Get daemon manager from app state
                if let Some(daemon_manager_mutex) =
                    window.try_state::<std::sync::Mutex<daemon_manager::DaemonManager>>()
                {
                    if let Ok(mut daemon_manager) = daemon_manager_mutex.lock() {
                        // Kill daemon if we spawned it (production mode only)
                        daemon_manager.kill_daemon();
                    }
                }

                // Only kill tmux sessions in production mode
                // In development, keep sessions alive for frontend hot reload reconnection
                if is_production {
                    eprintln!("[Loom] Production mode - cleaning up tmux sessions");
                    let _ = Command::new("tmux")
                        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
                        .output()
                        .map(|output| {
                            let sessions = String::from_utf8_lossy(&output.stdout);
                            for session in sessions.lines() {
                                if session.starts_with("loom-") {
                                    eprintln!("[Loom] Killing tmux session: {session}");
                                    let _ = Command::new("tmux")
                                        .args(["-L", "loom", "kill-session", "-t", session])
                                        .spawn();
                                }
                            }
                        });
                } else {
                    eprintln!(
                        "[Loom] Development mode - keeping tmux sessions alive for hot reload"
                    );
                }

                eprintln!("[Loom] Cleanup complete");
            }
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            check_system_dependencies,
            validate_git_repo,
            check_loom_initialized,
            initialize_loom_workspace,
            read_config,
            write_config,
            read_state,
            write_state,
            read_text_file,
            write_file,
            list_role_files,
            read_role_file,
            read_role_metadata,
            create_terminal,
            list_terminals,
            destroy_terminal,
            send_terminal_input,
            get_terminal_output,
            resize_terminal,
            check_session_health,
            check_daemon_health,
            get_daemon_status,
            list_available_sessions,
            attach_to_session,
            kill_session,
            set_worktree_path,
            get_env_var,
            check_claude_code,
            get_stored_workspace,
            set_stored_workspace,
            clear_stored_workspace,
            reset_workspace_to_defaults,
            check_github_remote,
            check_label_exists,
            create_github_label,
            update_github_label,
            reset_github_labels,
            create_local_project,
            create_github_repository,
            emit_menu_event,
            emit_event,
            append_to_console_log,
            trigger_start,
            trigger_force_start,
            trigger_factory_reset,
            kill_all_loom_sessions
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("Error while running tauri application: {e}");
        std::process::exit(1);
    }
}
