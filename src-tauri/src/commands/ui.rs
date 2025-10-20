/// Emit a menu event programmatically (for MCP control)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn emit_menu_event(window: tauri::Window, event_name: &str) -> Result<(), String> {
    window
        .emit(event_name, ())
        .map_err(|e| format!("Failed to emit event: {e}"))
}

/// Emit any event programmatically (for MCP file watcher)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn emit_event(window: tauri::Window, event: &str) -> Result<(), String> {
    window
        .emit(event, ())
        .map_err(|e| format!("Failed to emit event: {e}"))
}

/// Trigger start workspace via MCP
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn trigger_start(window: tauri::Window) -> Result<(), String> {
    emit_menu_event(window, "start-workspace")
}

/// Trigger force start workspace via MCP
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn trigger_force_start(window: tauri::Window) -> Result<(), String> {
    emit_menu_event(window, "force-start-workspace")
}

/// Trigger factory reset via MCP
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn trigger_factory_reset(window: tauri::Window) -> Result<(), String> {
    emit_menu_event(window, "factory-reset-workspace")
}
