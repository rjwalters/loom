use std::fs;
use std::path::Path;

#[tauri::command]
pub fn read_text_file(path: &str) -> Result<String, String> {
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
pub fn write_file(path: &str, content: &str) -> Result<(), String> {
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

#[tauri::command]
pub fn append_to_console_log(message: &str) -> Result<(), String> {
    use std::io::Write;

    let log_path = dirs::home_dir()
        .ok_or_else(|| "Failed to get home directory".to_string())?
        .join(".loom")
        .join("console.log");

    // Ensure .loom directory exists
    if let Some(parent) = log_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
        }
    }

    // Append message to log file
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("Failed to open console log: {e}"))?;

    writeln!(file, "{message}").map_err(|e| format!("Failed to write to console log: {e}"))?;

    Ok(())
}
