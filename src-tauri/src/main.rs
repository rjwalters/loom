// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use tauri::{CustomMenuItem, Menu, MenuItem, Submenu};

mod daemon_client;

use daemon_client::{DaemonClient, Request, Response, TerminalInfo};

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! Welcome to Loom.")
}

#[tauri::command]
async fn create_terminal(name: String, working_dir: Option<String>) -> Result<String, String> {
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
        Response::TerminalOutput { output, line_count } => {
            Ok(TerminalOutput { output, line_count })
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

#[tauri::command]
fn initialize_loom_workspace(path: &str, defaults_path: &str) -> Result<(), String> {
    let workspace_path = Path::new(path);
    let loom_path = workspace_path.join(".loom");

    // Check if .loom already exists
    if loom_path.exists() {
        return Err("Workspace already initialized (.loom directory exists)".to_string());
    }

    // Copy defaults to .loom (symlink in src-tauri/ points to ../defaults/)
    let defaults = Path::new(defaults_path);
    if !defaults.exists() {
        return Err(format!("Defaults directory not found: {defaults_path}"));
    }

    copy_dir_recursive(defaults, &loom_path)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Add .loom/ to .gitignore
    let gitignore_path = workspace_path.join(".gitignore");
    let loom_entry = ".loom/\n";

    // Check if .gitignore exists and if .loom/ is already in it
    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

        if !contents.contains(".loom/") {
            // Append .loom/ to .gitignore
            let mut new_contents = contents;
            if !new_contents.ends_with('\n') {
                new_contents.push('\n');
            }
            new_contents.push_str(loom_entry);

            fs::write(&gitignore_path, new_contents)
                .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
        }
    } else {
        // Create .gitignore with .loom/
        fs::write(&gitignore_path, loom_entry)
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

fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    // Build File menu
    let close_workspace =
        CustomMenuItem::new("close_workspace", "Close Workspace").accelerator("CmdOrCtrl+W");

    let file_menu = Submenu::new(
        "File",
        Menu::new()
            .add_item(close_workspace)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::Quit),
    );

    let menu = Menu::new().add_submenu(file_menu);

    if let Err(e) = tauri::Builder::default()
        .menu(menu)
        .on_menu_event(|event| {
            if event.menu_item_id() == "close_workspace" {
                // Emit event to frontend
                if let Err(e) = event.window().emit("close-workspace", ()) {
                    eprintln!("Failed to emit close-workspace event: {e}");
                }
            }
        })
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event.event() {
                // App is quitting - clean up all loom tmux sessions
                eprintln!("App closing - cleaning up tmux sessions...");

                // Kill all loom- tmux sessions
                let _ = Command::new("tmux")
                    .args(["list-sessions", "-F", "#{session_name}"])
                    .output()
                    .map(|output| {
                        let sessions = String::from_utf8_lossy(&output.stdout);
                        for session in sessions.lines() {
                            if session.starts_with("loom-") {
                                eprintln!("Killing tmux session: {session}");
                                let _ = Command::new("tmux")
                                    .args(["kill-session", "-t", session])
                                    .spawn();
                            }
                        }
                    });

                eprintln!("Cleanup complete");
            }
        })
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
            get_terminal_output,
            resize_terminal,
            check_session_health,
            list_available_sessions,
            attach_to_session,
            get_env_var,
            check_claude_code,
            get_stored_workspace,
            set_stored_workspace,
            clear_stored_workspace
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("Error while running tauri application: {e}");
        std::process::exit(1);
    }
}
