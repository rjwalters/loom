use crate::safe_eprintln;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;
use tauri::Window;

#[derive(Debug, Deserialize, Serialize)]
struct MCPCommand {
    command: String,
    timestamp: String,
}

/// Start watching for MCP commands in ~/.loom/mcp-command.json using filesystem events
pub fn start_mcp_watcher(window: Window) {
    // Spawn a background thread for the file watcher
    std::thread::spawn(move || {
        let command_file = get_command_file_path();

        // Get parent directory for watching
        let watch_dir = if let Some(dir) = command_file.parent() {
            dir
        } else {
            safe_eprintln!("[MCP Watcher] Failed to get parent directory of command file");
            safe_eprintln!("[MCP Watcher] Falling back to polling mode");
            fallback_polling_watcher(&window);
            return;
        };

        // Create a channel to receive file system events
        let (tx, rx) = channel();

        // Create file watcher (using recommended watcher for platform)
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                safe_eprintln!("[MCP Watcher] Failed to create file watcher: {e}");
                safe_eprintln!("[MCP Watcher] Falling back to polling mode");
                fallback_polling_watcher(&window);
                return;
            }
        };

        // Watch the .loom directory (not recursive - just the directory itself)
        if let Err(e) = watcher.watch(watch_dir, RecursiveMode::NonRecursive) {
            safe_eprintln!("[MCP Watcher] Failed to watch directory: {e}");
            safe_eprintln!("[MCP Watcher] Falling back to polling mode");
            drop(watcher);
            fallback_polling_watcher(&window);
            return;
        }

        safe_eprintln!("[MCP Watcher] Watching for file changes using notify crate");

        // Process file system events
        loop {
            match rx.recv() {
                Ok(Ok(event)) => {
                    // Only process events for our specific file
                    if !event_matches_command_file(&event, &command_file) {
                        continue;
                    }

                    // Handle the event
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Access(_)
                    ) {
                        // Small delay to ensure file is fully written
                        std::thread::sleep(Duration::from_millis(10));
                        process_command_file(&window, &command_file);
                    }
                }
                Ok(Err(e)) => {
                    safe_eprintln!("[MCP Watcher] File watch error: {e}");
                }
                Err(e) => {
                    safe_eprintln!("[MCP Watcher] Channel receive error: {e}");
                    break;
                }
            }
        }

        // Keep watcher alive by moving it to the end
        drop(watcher);
        safe_eprintln!("[MCP Watcher] File watcher stopped");
    });
}

/// Check if the file system event matches our command file
fn event_matches_command_file(event: &Event, command_file: &PathBuf) -> bool {
    event.paths.iter().any(|p| p == command_file)
}

/// Process the MCP command file
fn process_command_file(window: &Window, command_file: &PathBuf) {
    // Check if command file exists
    if !command_file.exists() {
        return;
    }

    // Read and process command
    let content = match fs::read_to_string(command_file) {
        Ok(c) => c,
        Err(e) => {
            safe_eprintln!("[MCP Watcher] Failed to read command file: {e}");
            return;
        }
    };

    let mcp_cmd = match serde_json::from_str::<MCPCommand>(&content) {
        Ok(cmd) => cmd,
        Err(e) => {
            safe_eprintln!("[MCP Watcher] Failed to parse command JSON: {e}");
            return;
        }
    };

    safe_eprintln!(
        "[MCP Watcher] Received command: {} (timestamp: {})",
        mcp_cmd.command,
        mcp_cmd.timestamp
    );

    // Execute the command
    let result = match mcp_cmd.command.as_str() {
        "trigger_start" => {
            safe_eprintln!("[MCP Watcher] Emitting start-workspace event");
            window.emit("start-workspace", ())
        }
        "trigger_force_start" => {
            safe_eprintln!("[MCP Watcher] Emitting force-start-workspace event");
            window.emit("force-start-workspace", ())
        }
        "trigger_factory_reset" => {
            safe_eprintln!("[MCP Watcher] Emitting factory-reset-workspace event");
            window.emit("factory-reset-workspace", ())
        }
        "trigger_force_factory_reset" => {
            safe_eprintln!("[MCP Watcher] Emitting force-factory-reset-workspace event");
            window.emit("force-factory-reset-workspace", ())
        }
        cmd if cmd.starts_with("restart_terminal:") => {
            // Parse terminal ID from command string
            let terminal_id = cmd.strip_prefix("restart_terminal:").unwrap_or("");
            safe_eprintln!(
                "[MCP Watcher] Emitting restart-terminal event for terminal: {terminal_id}"
            );
            window.emit("restart-terminal", terminal_id)
        }
        _ => {
            safe_eprintln!("[MCP Watcher] Unknown command: {}", mcp_cmd.command);
            Ok(())
        }
    };

    // Check success BEFORE consuming result
    let success = result.is_ok();

    if let Err(e) = result {
        safe_eprintln!("[MCP Watcher] Failed to execute command: {e}");
    } else {
        safe_eprintln!("[MCP Watcher] Command executed successfully");
    }

    // Write acknowledgment file BEFORE deleting command file
    // This signals to the MCP server that the command was processed
    let ack_file = get_ack_file_path();
    let ack_data = serde_json::json!({
        "command": mcp_cmd.command,
        "timestamp": mcp_cmd.timestamp,
        "processed_at": chrono::Utc::now().to_rfc3339(),
        "success": success,
    });
    if let Err(e) =
        fs::write(&ack_file, serde_json::to_string_pretty(&ack_data).unwrap_or_default())
    {
        safe_eprintln!("[MCP Watcher] Failed to write acknowledgment file: {e}");
    }

    // Delete the command file after processing
    if let Err(e) = fs::remove_file(command_file) {
        safe_eprintln!("[MCP Watcher] Failed to remove command file: {e}");
    }
}

/// Fallback to polling if notify fails to initialize
fn fallback_polling_watcher(window: &Window) {
    use std::time::SystemTime;

    let mut last_processed: Option<SystemTime> = None;
    let command_file = get_command_file_path();

    loop {
        // Poll every 500ms
        std::thread::sleep(Duration::from_millis(500));

        // Check if command file exists
        if !command_file.exists() {
            continue;
        }

        // Get file metadata to check modification time
        if let Ok(metadata) = fs::metadata(&command_file) {
            if let Ok(modified) = metadata.modified() {
                // If we haven't processed this file yet, or if it's been modified since last check
                let should_process = last_processed.map_or(true, |last| last < modified);
                if should_process {
                    process_command_file(window, &command_file);
                    last_processed = Some(modified);
                }
            }
        }
    }
}

#[allow(clippy::expect_used)]
fn get_command_file_path() -> PathBuf {
    // Home directory must exist for the app to function
    // This is only called in background thread, panic is acceptable
    dirs::home_dir()
        .expect("Could not get home directory")
        .join(".loom")
        .join("mcp-command.json")
}

#[allow(clippy::expect_used)]
fn get_ack_file_path() -> PathBuf {
    // Home directory must exist for the app to function
    // This is only called in background thread, panic is acceptable
    dirs::home_dir()
        .expect("Could not get home directory")
        .join(".loom")
        .join("mcp-ack.json")
}
