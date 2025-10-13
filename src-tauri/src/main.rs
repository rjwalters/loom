// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use tauri::{CustomMenuItem, Manager, Menu, MenuItem, Submenu};

mod daemon_client;
mod daemon_manager;

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
        .map(|h| h.join(".loom/daemon.sock"))
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
                            role_files.push(name.to_string());
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
    let defaults = Path::new(defaults_path);
    if !defaults.exists() {
        return Err(format!("Defaults directory not found: {defaults_path}"));
    }

    copy_dir_recursive(defaults, &loom_path)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Add .loom/ to .gitignore (ensures it's present on both initialization and factory reset)
    let gitignore_path = workspace.join(".gitignore");
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

fn build_menu() -> Menu {
    // Build File menu
    let new_terminal =
        CustomMenuItem::new("new_terminal", "New Terminal").accelerator("CmdOrCtrl+T");
    let close_terminal =
        CustomMenuItem::new("close_terminal", "Close Terminal").accelerator("CmdOrCtrl+Shift+W");
    let close_workspace =
        CustomMenuItem::new("close_workspace", "Close Workspace").accelerator("CmdOrCtrl+W");
    let factory_reset_workspace =
        CustomMenuItem::new("factory_reset_workspace", "Factory Reset Workspace...");

    let file_menu = Submenu::new(
        "File",
        Menu::new()
            .add_item(new_terminal)
            .add_item(close_terminal)
            .add_native_item(MenuItem::Separator)
            .add_item(close_workspace)
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

fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    let menu = build_menu();

    if let Err(e) = tauri::Builder::default()
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

            Ok(())
        })
        .menu(menu)
        .on_menu_event(|event| handle_menu_event(&event))
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event.event() {
                // App is quitting - clean up resources
                eprintln!("[Loom] App closing - cleaning up...");

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
                    eprintln!("[Loom] Production mode - cleaning up tmux sessions");
                    let _ = Command::new("tmux")
                        .args(["list-sessions", "-F", "#{session_name}"])
                        .output()
                        .map(|output| {
                            let sessions = String::from_utf8_lossy(&output.stdout);
                            for session in sessions.lines() {
                                if session.starts_with("loom-") {
                                    eprintln!("[Loom] Killing tmux session: {session}");
                                    let _ = Command::new("tmux")
                                        .args(["kill-session", "-t", session])
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
            validate_git_repo,
            check_loom_initialized,
            initialize_loom_workspace,
            read_config,
            write_config,
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
            get_env_var,
            check_claude_code,
            get_stored_workspace,
            set_stored_workspace,
            clear_stored_workspace,
            reset_workspace_to_defaults
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("Error while running tauri application: {e}");
        std::process::exit(1);
    }
}
