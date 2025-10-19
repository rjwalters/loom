// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::Datelike;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use tauri::{CustomMenuItem, Manager, Menu, MenuItem, Submenu};

mod daemon_client;
mod daemon_manager;
mod dependency_checker;
mod logging;
mod mcp_watcher;

use daemon_client::{DaemonClient, Request, Response, TerminalInfo};

// App state to hold CLI workspace argument
#[derive(Default)]
struct CliWorkspace(std::sync::Mutex<Option<String>>);

// Helper structs for JSON parsing (defined at top level to avoid clippy::items_after_statements)
#[derive(serde::Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(serde::Deserialize)]
struct GhIssue {
    number: u32,
}

#[derive(serde::Deserialize)]
struct GhPr {
    number: u32,
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! Welcome to Loom.")
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn get_cli_workspace(cli_workspace: tauri::State<CliWorkspace>) -> Option<String> {
    cli_workspace.0.lock().ok()?.clone()
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

// Helper function to setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
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

    Ok(())
}

/// Ensure repository scaffolding is set up for an existing workspace
///
/// This function should be called when attaching to an existing repository to ensure
/// that necessary files (.claude/, CLAUDE.md, .codex/, AGENTS.md) exist.
///
/// Unlike init_loom_directory, this ONLY sets up the scaffolding, not the .loom/ directory.
#[tauri::command]
fn ensure_workspace_scaffolding(workspace_path: &str) -> Result<(), String> {
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

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace_path, &defaults)?;

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
        .path_resolver()
        .app_data_dir()
        .ok_or_else(|| "Failed to get app data directory".to_string())?;

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

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    setup_repository_scaffolding(workspace, &defaults)?;

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
        .args(["label", "list", "--json", "name"])
        .output()
        .map_err(|e| format!("Failed to run gh label list: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label list failed: {stderr}"));
    }

    // Parse JSON in Rust instead of using jq to prevent injection
    let labels: Vec<GhLabel> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse label JSON: {e}"))?;

    Ok(labels.iter().any(|l| l.name == name))
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
        ])
        .output()
        .map_err(|e| format!("Failed to list issues: {e}"))?;

    if issues_output.status.success() {
        // Parse JSON in Rust instead of using jq to prevent injection
        let issues: Vec<GhIssue> = serde_json::from_slice(&issues_output.stdout)
            .map_err(|e| format!("Failed to parse issue JSON: {e}"))?;

        for issue in issues {
            let issue_num = issue.number.to_string();

            let remove_output = Command::new("gh")
                .args([
                    "issue",
                    "edit",
                    &issue_num,
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
        ])
        .output()
        .map_err(|e| format!("Failed to list PRs: {e}"))?;

    if prs_output.status.success() {
        // Parse JSON in Rust instead of using jq to prevent injection
        let prs: Vec<GhPr> = serde_json::from_slice(&prs_output.stdout)
            .map_err(|e| format!("Failed to parse PR JSON: {e}"))?;

        for pr in prs {
            let pr_num = pr.number.to_string();

            let edit_output = Command::new("gh")
                .args([
                    "pr",
                    "edit",
                    &pr_num,
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

    // Create initial .gitignore with Loom ephemeral file patterns
    let gitignore_content = "# Loom - AI Development Orchestration\n\
                             .loom/state.json\n\
                             .loom/worktrees/\n\
                             .loom/*.log\n\
                             .loom/*.sock\n";
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

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    setup_repository_scaffolding(project_path, &defaults_dir)?;

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
    safe_eprintln!("[kill_all_loom_sessions] Killing all loom tmux sessions");

    let output = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(|e| format!("Failed to list tmux sessions: {e}"))?;

    if !output.status.success() {
        // tmux list-sessions fails if no sessions exist - this is OK
        safe_eprintln!("[kill_all_loom_sessions] No tmux sessions found");
        return Ok(());
    }

    let sessions = String::from_utf8_lossy(&output.stdout);
    let mut killed_count = 0;

    for session in sessions.lines() {
        if session.starts_with("loom-") {
            safe_eprintln!("[kill_all_loom_sessions] Killing tmux session: {session}");

            let kill_output = Command::new("tmux")
                .args(["-L", "loom", "kill-session", "-t", session])
                .output()
                .map_err(|e| format!("Failed to kill session {session}: {e}"))?;

            if kill_output.status.success() {
                killed_count += 1;
            } else {
                safe_eprintln!(
                    "[kill_all_loom_sessions] Failed to kill {session}: {}",
                    String::from_utf8_lossy(&kill_output.stderr)
                );
            }
        }
    }

    safe_eprintln!("[kill_all_loom_sessions] Killed {killed_count} sessions");
    Ok(())
}

fn build_menu() -> Menu {
    // Build File menu
    let new_terminal =
        CustomMenuItem::new("new_terminal", "New Terminal").accelerator("CmdOrCtrl+T");
    let close_terminal =
        CustomMenuItem::new("close_terminal", "Close Terminal").accelerator("CmdOrCtrl+Shift+W");
    let close_workspace =
        CustomMenuItem::new("close_workspace", "Close Workspace").accelerator("CmdOrCtrl+W");
    let start_workspace =
        CustomMenuItem::new("start_workspace", "Start...").accelerator("CmdOrCtrl+Shift+R");
    let force_start_workspace = CustomMenuItem::new("force_start_workspace", "Force Start")
        .accelerator("CmdOrCtrl+Shift+Alt+R");
    let factory_reset_workspace =
        CustomMenuItem::new("factory_reset_workspace", "Factory Reset...");

    let file_menu = Submenu::new(
        "File",
        Menu::new()
            .add_item(new_terminal)
            .add_item(close_terminal)
            .add_native_item(MenuItem::Separator)
            .add_item(close_workspace)
            .add_item(start_workspace)
            .add_item(force_start_workspace)
            .add_item(factory_reset_workspace)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::Quit),
    );

    // Build Edit menu
    let edit_menu = Submenu::new(
        "Edit",
        Menu::new()
            .add_native_item(MenuItem::Cut)
            .add_native_item(MenuItem::Copy)
            .add_native_item(MenuItem::Paste)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::SelectAll),
    );

    // Build View menu
    let toggle_theme =
        CustomMenuItem::new("toggle_theme", "Toggle Theme").accelerator("CmdOrCtrl+Shift+T");
    let zoom_in = CustomMenuItem::new("zoom_in", "Zoom In").accelerator("CmdOrCtrl+=");
    let zoom_out = CustomMenuItem::new("zoom_out", "Zoom Out").accelerator("CmdOrCtrl+-");
    let reset_zoom = CustomMenuItem::new("reset_zoom", "Reset Zoom").accelerator("CmdOrCtrl+0");

    let view_menu = Submenu::new(
        "View",
        Menu::new()
            .add_item(toggle_theme)
            .add_native_item(MenuItem::Separator)
            .add_item(zoom_in)
            .add_item(zoom_out)
            .add_item(reset_zoom)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::EnterFullScreen),
    );

    // Build Window menu
    let window_menu = Submenu::new(
        "Window",
        Menu::new()
            .add_native_item(MenuItem::Minimize)
            .add_native_item(MenuItem::Zoom),
    );

    // Build Help menu
    let documentation = CustomMenuItem::new("documentation", "Documentation");
    let view_github = CustomMenuItem::new("view_github", "View on GitHub");
    let report_issue = CustomMenuItem::new("report_issue", "Report Issue");
    let daemon_status = CustomMenuItem::new("daemon_status", "Daemon Status...");
    let keyboard_shortcuts =
        CustomMenuItem::new("keyboard_shortcuts", "Keyboard Shortcuts").accelerator("CmdOrCtrl+/");

    let help_menu = Submenu::new(
        "Help",
        Menu::new()
            .add_item(documentation)
            .add_item(view_github)
            .add_item(report_issue)
            .add_native_item(MenuItem::Separator)
            .add_item(daemon_status)
            .add_item(keyboard_shortcuts),
    );

    Menu::new()
        .add_submenu(file_menu)
        .add_submenu(edit_menu)
        .add_submenu(view_menu)
        .add_submenu(window_menu)
        .add_submenu(help_menu)
}

fn handle_menu_event(event: &tauri::WindowMenuEvent) {
    let menu_id = event.menu_item_id();

    match menu_id {
        "new_terminal" => {
            let _ = event.window().emit("new-terminal", ());
        }
        "close_terminal" => {
            let _ = event.window().emit("close-terminal", ());
        }
        "close_workspace" => {
            let _ = event.window().emit("close-workspace", ());
        }
        "start_workspace" => {
            let _ = event.window().emit("start-workspace", ());
        }
        "force_start_workspace" => {
            let _ = event.window().emit("force-start-workspace", ());
        }
        "factory_reset_workspace" => {
            let _ = event.window().emit("factory-reset-workspace", ());
        }
        "toggle_theme" => {
            let _ = event.window().emit("toggle-theme", ());
        }
        "zoom_in" => {
            let _ = event.window().emit("zoom-in", ());
        }
        "zoom_out" => {
            let _ = event.window().emit("zoom-out", ());
        }
        "reset_zoom" => {
            let _ = event.window().emit("reset-zoom", ());
        }
        "documentation" => {
            let _ = tauri::api::shell::open(
                &event.window().shell_scope(),
                "https://github.com/rjwalters/loom#readme",
                None,
            );
        }
        "view_github" => {
            let _ = tauri::api::shell::open(
                &event.window().shell_scope(),
                "https://github.com/rjwalters/loom",
                None,
            );
        }
        "report_issue" => {
            let _ = tauri::api::shell::open(
                &event.window().shell_scope(),
                "https://github.com/rjwalters/loom/issues/new",
                None,
            );
        }
        "keyboard_shortcuts" => {
            let _ = event.window().emit("show-shortcuts", ());
        }
        "daemon_status" => {
            let _ = event.window().emit("show-daemon-status", ());
        }
        _ => {}
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    let menu = build_menu();

    // Create CLI workspace state
    let cli_workspace_state = CliWorkspace::default();

    if let Err(e) = tauri::Builder::default()
        .setup(|app| {
            // Check if we're in development or production mode
            let is_production = !cfg!(debug_assertions);

            safe_eprintln!(
                "[Loom] Starting in {} mode",
                if is_production {
                    "production"
                } else {
                    "development"
                }
            );

            // Handle CLI arguments
            match app.get_cli_matches() {
                Ok(matches) => {
                    // Check for --workspace argument
                    if let Some(workspace_arg) = matches.args.get("workspace") {
                        if let serde_json::Value::String(workspace_path) = &workspace_arg.value {
                            safe_eprintln!("[Loom] CLI workspace argument: {workspace_path}");

                            // Store in app state for synchronous access
                            if let Some(cli_ws) = app.try_state::<CliWorkspace>() {
                                if let Ok(mut cli_ws_lock) = cli_ws.0.lock() {
                                    *cli_ws_lock = Some(workspace_path.clone());
                                }
                            }

                            // Get the main window
                            if let Some(window) = app.get_window("main") {
                                // Emit event to frontend with workspace path (for backward compatibility)
                                window.emit("cli-workspace", workspace_path).map_err(|e| {
                                    format!("Failed to emit cli-workspace event: {e}")
                                })?;
                            }
                        }
                    }
                }
                Err(e) => {
                    safe_eprintln!("[Loom] Warning: Failed to parse CLI arguments: {e}");
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
                .get_window("main")
                .ok_or_else(|| "Failed to get main window".to_string())?;
            mcp_watcher::start_mcp_watcher(window);
            safe_eprintln!("[Loom] MCP command watcher started");

            Ok(())
        })
        .manage(cli_workspace_state)
        .menu(menu)
        .on_menu_event(|event| handle_menu_event(&event))
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event.event() {
                // App is quitting - clean up resources
                safe_eprintln!("[Loom] App closing - cleaning up...");

                let is_production = !cfg!(debug_assertions);

                // Get daemon manager from app state
                if let Some(daemon_manager_mutex) = event
                    .window()
                    .try_state::<std::sync::Mutex<daemon_manager::DaemonManager>>()
                {
                    if let Ok(mut daemon_manager) = daemon_manager_mutex.lock() {
                        // Kill daemon if we spawned it (production mode only)
                        daemon_manager.kill_daemon();
                    }
                }

                // Only kill tmux sessions in production mode
                // In development, keep sessions alive for frontend hot reload reconnection
                if is_production {
                    safe_eprintln!("[Loom] Production mode - cleaning up tmux sessions");
                    let _ = Command::new("tmux")
                        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
                        .output()
                        .map(|output| {
                            let sessions = String::from_utf8_lossy(&output.stdout);
                            for session in sessions.lines() {
                                if session.starts_with("loom-") {
                                    safe_eprintln!("[Loom] Killing tmux session: {session}");
                                    let _ = Command::new("tmux")
                                        .args(["-L", "loom", "kill-session", "-t", session])
                                        .spawn();
                                }
                            }
                        });
                } else {
                    safe_eprintln!(
                        "[Loom] Development mode - keeping tmux sessions alive for hot reload"
                    );
                }

                safe_eprintln!("[Loom] Cleanup complete");
            }
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            get_cli_workspace,
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
            ensure_workspace_scaffolding,
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
        safe_eprintln!("Error while running tauri application: {e}");
        std::process::exit(1);
    }
}
