// These modules were originally private to the binary crate. Exposing them as
// a library (to allow unit tests to run without the binary's tokio runtime)
// triggers public-API clippy lints that don't apply to internal-use code.
#![allow(clippy::must_use_candidate)]
#![allow(clippy::should_implement_trait)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::new_without_default)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

pub mod activity;
pub mod errors;
pub mod git_parser;
pub mod git_utils;
pub mod github_parser;
pub mod health_monitor;
pub mod init;
pub mod ipc;
pub mod metrics_collector;
pub mod role_validation;
pub mod terminal;
pub mod types;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Rotate log file if it exceeds max size.
/// Keeps last `max_files` rotated files (log.1, log.2, ..., log.N).
pub fn rotate_log_file(log_path: &Path, max_size: u64, max_files: usize) -> anyhow::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(log_path)?;
    if metadata.len() < max_size {
        return Ok(());
    }

    // Remove oldest rotated file if it exists
    let oldest_file = format!("{}.{max_files}", log_path.display());
    let _ = fs::remove_file(&oldest_file);

    // Shift existing rotated files (log.N-1 -> log.N, etc.)
    for i in (1..max_files).rev() {
        let old_path = format!("{}.{i}", log_path.display());
        let new_path = format!("{}.{}", log_path.display(), i + 1);
        if Path::new(&old_path).exists() {
            let _ = fs::rename(&old_path, &new_path);
        }
    }

    // Rotate current log file to log.1
    let rotated_path = format!("{}.1", log_path.display());
    fs::rename(log_path, rotated_path)?;

    Ok(())
}

/// Extract configured terminal IDs from workspace config.json.
///
/// Reads the workspace's `.loom/config.json` and extracts the `id` field from
/// each terminal entry. Returns None if config file doesn't exist or can't be parsed.
pub fn extract_configured_terminal_ids(workspace: &Path) -> Option<HashSet<String>> {
    let config_path = workspace.join(".loom").join("config.json");

    let config_str = match fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!("Could not read config at {}: {e}", config_path.display());
            return None;
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Could not parse config at {}: {e}", config_path.display());
            return None;
        }
    };

    let terminals = config.get("terminals")?.as_array()?;

    let ids: HashSet<String> = terminals
        .iter()
        .filter_map(|t| t.get("id")?.as_str().map(String::from))
        .collect();

    if ids.is_empty() {
        log::debug!("No terminal IDs found in config");
        return None;
    }

    log::info!("Loaded {} configured terminal IDs from {}", ids.len(), config_path.display());

    Some(ids)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ===== rotate_log_file tests =====

    #[test]
    fn test_rotate_log_file_no_file_exists() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");
        rotate_log_file(&log_path, 1024, 10).unwrap();
    }

    #[test]
    fn test_rotate_log_file_under_limit() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");
        fs::write(&log_path, "small content").unwrap();

        rotate_log_file(&log_path, 1024 * 1024, 10).unwrap();

        assert!(log_path.exists());
        assert_eq!(fs::read_to_string(&log_path).unwrap(), "small content");
    }

    #[test]
    fn test_rotate_log_file_at_limit() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        let content = "x".repeat(100);
        fs::write(&log_path, &content).unwrap();

        rotate_log_file(&log_path, 50, 5).unwrap();

        assert!(!log_path.exists());
        let rotated = dir.path().join("daemon.log.1");
        assert!(rotated.exists());
        assert_eq!(fs::read_to_string(rotated).unwrap(), content);
    }

    #[test]
    fn test_rotate_log_file_shifts_existing() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        fs::write(dir.path().join("daemon.log.1"), "old content").unwrap();
        fs::write(&log_path, "x".repeat(100)).unwrap();

        rotate_log_file(&log_path, 50, 5).unwrap();

        assert!(dir.path().join("daemon.log.2").exists());
        assert_eq!(fs::read_to_string(dir.path().join("daemon.log.2")).unwrap(), "old content");
        assert!(dir.path().join("daemon.log.1").exists());
    }

    #[test]
    fn test_rotate_log_file_removes_oldest() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        fs::write(dir.path().join("daemon.log.3"), "oldest").unwrap();
        fs::write(&log_path, "x".repeat(100)).unwrap();

        rotate_log_file(&log_path, 50, 3).unwrap();

        assert!(dir.path().join("daemon.log.1").exists());
    }

    // ===== extract_configured_terminal_ids tests =====

    #[test]
    fn test_extract_terminal_ids_missing_config() {
        let dir = tempdir().unwrap();
        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_invalid_json() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), "not valid json").unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_no_terminals_key() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), r#"{"other": "data"}"#).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_empty_terminals() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), r#"{"terminals": []}"#).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_valid_config() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();

        let config = r#"{
            "nextAgentNumber": 3,
            "terminals": [
                {"id": "terminal-1", "name": "Builder", "role": "builder"},
                {"id": "terminal-2", "name": "Judge", "role": "judge"},
                {"id": "shepherd-1", "name": "Shepherd", "role": "shepherd"}
            ]
        }"#;
        fs::write(loom_dir.join("config.json"), config).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_some());
        let ids = result.unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("terminal-1"));
        assert!(ids.contains("terminal-2"));
        assert!(ids.contains("shepherd-1"));
    }

    #[test]
    fn test_extract_terminal_ids_skips_entries_without_id() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();

        let config = r#"{
            "terminals": [
                {"id": "terminal-1", "name": "Builder"},
                {"name": "No ID"},
                {"id": "terminal-3", "name": "Third"}
            ]
        }"#;
        fs::write(loom_dir.join("config.json"), config).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_some());
        let ids = result.unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("terminal-1"));
        assert!(ids.contains("terminal-3"));
    }
}
