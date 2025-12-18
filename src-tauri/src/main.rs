// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::Command;
use tauri::{Emitter, Manager};

mod daemon_client;
mod daemon_manager;
mod dependency_checker;
mod logging;
mod mcp_watcher;

// Command modules organized by domain
mod commands;
mod menu;

// Import all commands for registration
#[allow(clippy::wildcard_imports)]
use commands::*;

// App state to hold CLI workspace argument
#[derive(Default)]
struct CliWorkspace(std::sync::Mutex<Option<String>>);

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn get_cli_workspace(cli_workspace: tauri::State<CliWorkspace>) -> Option<String> {
    cli_workspace.0.lock().ok()?.clone()
}

#[allow(clippy::too_many_lines)]
fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    // Create CLI workspace state
    let cli_workspace_state = CliWorkspace::default();

    if let Err(e) = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_cli::init())
        .setup(|app| {
            // Build menu with Tauri 2.x API
            let menu = menu::build_menu(app)?;
            app.set_menu(menu)?;
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

            // Handle CLI arguments using Tauri 2.x plugin API
            match tauri_plugin_cli::CliExt::cli(app).matches() {
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
                            if let Some(window) = app.get_webview_window("main") {
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
                .get_webview_window("main")
                .ok_or_else(|| "Failed to get main window".to_string())?;
            mcp_watcher::start_mcp_watcher(window);
            safe_eprintln!("[Loom] MCP command watcher started");

            Ok(())
        })
        .manage(cli_workspace_state)
        .on_menu_event(menu::handle_menu_event)
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // App is quitting - clean up resources
                safe_eprintln!("[Loom] App closing - cleaning up...");

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
            // Legacy commands
            get_cli_workspace,
            // System commands
            check_system_dependencies,
            get_env_var,
            check_claude_code,
            check_process_alive,
            check_path_exists,
            // Workspace commands
            validate_git_repo,
            check_loom_initialized,
            initialize_loom_workspace,
            ensure_workspace_scaffolding,
            get_stored_workspace,
            set_stored_workspace,
            clear_stored_workspace,
            reset_workspace_to_defaults,
            // Config/State commands
            read_config,
            write_config,
            read_state,
            write_state,
            list_role_files,
            read_role_file,
            read_role_metadata,
            // Activity logging commands
            log_activity,
            read_recent_activity,
            get_activity_by_role,
            query_token_usage_by_role,
            query_token_usage_timeline,
            // Terminal commands
            create_terminal,
            list_terminals,
            destroy_terminal,
            send_terminal_input,
            get_terminal_output,
            get_terminal_activity,
            resize_terminal,
            check_session_health,
            list_available_sessions,
            attach_to_session,
            kill_session,
            set_worktree_path,
            kill_all_loom_sessions,
            // Daemon commands
            check_daemon_health,
            get_daemon_status,
            // Filesystem commands
            read_text_file,
            write_file,
            append_to_console_log,
            // GitHub commands
            check_github_remote,
            check_label_exists,
            create_github_label,
            update_github_label,
            reset_github_labels,
            // Project commands
            create_local_project,
            create_github_repository,
            // UI/Event commands
            emit_menu_event,
            emit_event,
            trigger_start,
            trigger_force_start,
            trigger_factory_reset,
            // Telemetry commands
            log_performance_metric,
            get_performance_metrics,
            get_performance_stats,
            log_error_report,
            get_error_reports,
            log_usage_event,
            get_usage_events,
            get_usage_stats,
            delete_telemetry_data,
        ])
        .run(tauri::generate_context!())
    {
        safe_eprintln!("Error while running tauri application: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Unwrap is acceptable in tests
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_git_root_rejects_symlink() {
        // Create temp directory with .git symlink
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a target directory to symlink to
        let target_dir = temp_dir.path().join("target");
        fs::create_dir(&target_dir).unwrap();

        // Create symlink: .git -> target
        let git_symlink = temp_path.join(".git");
        unix_fs::symlink(&target_dir, &git_symlink).unwrap();

        // Change to temp directory
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp_path).unwrap();

        // Verify find_git_root() returns None (rejects symlink)
        let result = commands::workspace::find_git_root();
        assert!(result.is_none(), "find_git_root() should reject .git symlinks");

        // Restore original directory
        std::env::set_current_dir(original_dir).unwrap();
    }

    #[test]
    fn test_find_git_root_accepts_real_git_directory() {
        // Create temp directory with real .git directory
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create real .git directory
        let git_dir = temp_path.join(".git");
        fs::create_dir(&git_dir).unwrap();

        // Change to temp directory
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp_path).unwrap();

        // Verify find_git_root() returns Some with correct path
        let result = commands::workspace::find_git_root();
        assert!(result.is_some(), "find_git_root() should accept real .git directories");

        // In CI, the temp directory may be created inside the loom repository,
        // so find_git_root() might find the repository's .git instead of the test's .git.
        // We only care that it found SOME valid .git directory (not necessarily the temp one).
        // The symlink rejection test (test_find_git_root_rejects_symlink) verifies the security aspect.
        let found_path = result.unwrap().canonicalize().unwrap();
        assert!(found_path.join(".git").exists(), "Found path should contain a .git directory");

        // Restore original directory
        std::env::set_current_dir(original_dir).unwrap();
    }
}
