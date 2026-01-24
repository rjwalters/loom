use tauri::AppHandle;

/// Simple greeting command to demonstrate Tauri IPC
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! Welcome to Loom Quickstart.", name)
}

/// Get the app data directory path
#[tauri::command]
pub fn get_app_data_dir(app: AppHandle) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;

    // Ensure directory exists
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;

    path.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "Invalid path".to_string())
}
