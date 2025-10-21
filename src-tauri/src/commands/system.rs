use std::path::Path;
use std::process::Command;

#[tauri::command]
pub fn check_system_dependencies() -> crate::dependency_checker::DependencyStatus {
    crate::dependency_checker::check_dependencies()
}

#[tauri::command]
pub fn get_env_var(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("{key} not set in .env"))
}

#[tauri::command]
pub fn check_claude_code() -> Result<bool, String> {
    Command::new("which")
        .arg("claude-code")
        .output()
        .map(|o| o.status.success())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn check_process_alive(pid: u32) -> Result<bool, String> {
    // Use kill -0 which checks if process exists without sending a signal
    let output = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to check process: {e}"))?;

    // kill -0 returns success (0) if process exists, error otherwise
    Ok(output.status.success())
}

#[tauri::command]
pub fn check_path_exists(path: &str) -> Result<(), String> {
    let file_path = Path::new(path);

    if !file_path.exists() {
        return Err(format!("Path does not exist: {path}"));
    }

    Ok(())
}
