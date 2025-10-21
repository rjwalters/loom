use std::fs;
use std::path::Path;

#[tauri::command]
pub fn read_config(workspace_path: &str) -> Result<String, String> {
    let config_path = Path::new(workspace_path).join(".loom").join("config.json");

    if !config_path.exists() {
        return Err("Config file does not exist".to_string());
    }

    fs::read_to_string(&config_path).map_err(|e| format!("Failed to read config: {e}"))
}

#[tauri::command]
pub fn write_config(workspace_path: &str, config_json: String) -> Result<(), String> {
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
pub fn read_state(workspace_path: &str) -> Result<String, String> {
    let state_path = Path::new(workspace_path).join(".loom").join("state.json");

    if !state_path.exists() {
        return Err("State file does not exist".to_string());
    }

    fs::read_to_string(&state_path).map_err(|e| format!("Failed to read state: {e}"))
}

#[tauri::command]
pub fn write_state(workspace_path: &str, state_json: String) -> Result<(), String> {
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
pub fn list_role_files(workspace_path: &str) -> Result<Vec<String>, String> {
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
pub fn read_role_file(workspace_path: &str, filename: &str) -> Result<String, String> {
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
pub fn read_role_metadata(workspace_path: &str, filename: &str) -> Result<Option<String>, String> {
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
