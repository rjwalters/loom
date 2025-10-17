use crate::safe_eprintln;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tauri::Window;

#[derive(Debug, Deserialize, Serialize)]
struct MCPCommand {
    command: String,
    timestamp: String,
}

/// Start watching for MCP commands in ~/.loom/mcp-command.json
pub fn start_mcp_watcher(window: Window) {
    // Spawn a background thread to poll for commands
    std::thread::spawn(move || {
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
                        // Read and process command
                        if let Ok(content) = fs::read_to_string(&command_file) {
                            if let Ok(mcp_cmd) = serde_json::from_str::<MCPCommand>(&content) {
                                safe_eprintln!(
                                    "[MCP Watcher] Received command: {} (timestamp: {})",
                                    mcp_cmd.command,
                                    mcp_cmd.timestamp
                                );

                                // Execute the command
                                let result = match mcp_cmd.command.as_str() {
                                    "trigger_start" => {
                                        safe_eprintln!(
                                            "[MCP Watcher] Emitting start-workspace event"
                                        );
                                        window.emit("start-workspace", ())
                                    }
                                    "trigger_force_start" => {
                                        safe_eprintln!(
                                            "[MCP Watcher] Emitting force-start-workspace event"
                                        );
                                        window.emit("force-start-workspace", ())
                                    }
                                    "trigger_factory_reset" => {
                                        safe_eprintln!(
                                            "[MCP Watcher] Emitting factory-reset-workspace event"
                                        );
                                        window.emit("factory-reset-workspace", ())
                                    }
                                    "trigger_force_factory_reset" => {
                                        safe_eprintln!(
                                            "[MCP Watcher] Emitting force-factory-reset-workspace event"
                                        );
                                        window.emit("force-factory-reset-workspace", ())
                                    }
                                    _ => {
                                        safe_eprintln!(
                                            "[MCP Watcher] Unknown command: {}",
                                            mcp_cmd.command
                                        );
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

                                // Update last processed time
                                last_processed = Some(modified);

                                // Write acknowledgment file BEFORE deleting command file
                                // This signals to the MCP server that the command was processed
                                let ack_file = get_ack_file_path();
                                let ack_data = serde_json::json!({
                                    "command": mcp_cmd.command,
                                    "timestamp": mcp_cmd.timestamp,
                                    "processed_at": chrono::Utc::now().to_rfc3339(),
                                    "success": success,
                                });
                                if let Err(e) = fs::write(
                                    &ack_file,
                                    serde_json::to_string_pretty(&ack_data).unwrap_or_default(),
                                ) {
                                    safe_eprintln!(
                                        "[MCP Watcher] Failed to write acknowledgment file: {e}"
                                    );
                                }

                                // Delete the command file after processing
                                if let Err(e) = fs::remove_file(&command_file) {
                                    safe_eprintln!(
                                        "[MCP Watcher] Failed to remove command file: {e}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    });
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
