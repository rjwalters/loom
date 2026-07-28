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
pub mod capacity;
pub mod claim_reconciliation;
pub mod config_resolver;
pub mod cpu_headroom;
pub mod credential_preflight;
pub mod daemon_heartbeat;
pub mod disk_headroom;
pub mod epic_state;
pub mod epic_supervisor;
pub mod errors;
pub mod event_bus;
pub mod forge_parser;
pub mod git_parser;
pub mod git_utils;
pub mod health_monitor;
pub mod init;
pub mod ipc;
pub mod issue_creation_mutex;
pub mod main_health_gate;
pub mod metrics_collector;
pub mod phase_join;
pub mod pipeline_snapshot;
pub mod quarantine_reconciliation;
pub mod role_runner;
pub mod role_validation;
pub mod safehouse;
pub mod self_update;
pub mod sweep_journal;
pub mod sweep_registry;
pub mod terminal;
pub mod token_ranking_refresh;
pub mod tokens;
pub mod tokens_pool;
pub mod types;
pub mod watch_registry;
pub mod work_finder;
pub mod workspace_pool;
pub mod workspace_registry;
pub mod worktree_root;

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

/// Extract configured terminal IDs from workspace config.
///
/// Resolves the workspace's effective config through the tier chain
/// (`config_resolver::resolve_effective_config`: private/shared defaults →
/// legacy `.loom/config.json` → `.loom-project/project.json` →
/// `.loom-local/local.json`) and extracts the `id` field from each terminal
/// entry. Returns `None` when no tier supplies a non-empty `terminals` array —
/// the empty-set-⇒-`None` contract is preserved (#4059).
///
/// **Diagnostic note (#4059, Finding 3):** the previous direct-read
/// implementation emitted a function-local `log::warn!` for a malformed
/// `.loom/config.json`. That warn is not lost: `config_resolver`'s
/// `soft_read_json_object` still emits a `log::warn!` when a tier's JSON fails
/// to parse. A malformed config therefore collapses to `{}` at the resolver
/// layer (so it now reaches the "no terminals" branch here rather than a
/// dedicated parse-error branch), but the operator still sees a warning — it
/// just names the resolver tier rather than this call site. This diagnosability
/// change is accepted deliberately; the observable `None` contract is unchanged
/// and the existing invalid-JSON test guards the collapsed path.
pub fn extract_configured_terminal_ids(workspace: &Path) -> Option<HashSet<String>> {
    let config = config_resolver::resolve_effective_config(workspace);

    let terminals = config.get("terminals")?.as_array()?;

    let ids: HashSet<String> = terminals
        .iter()
        .filter_map(|t| t.get("id")?.as_str().map(String::from))
        .collect();

    if ids.is_empty() {
        log::debug!("No terminal IDs found in resolved config for {}", workspace.display());
        return None;
    }

    log::info!(
        "Loaded {} configured terminal IDs from resolved config for {}",
        ids.len(),
        workspace.display()
    );

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

    /// #4059: terminals supplied ONLY by the `.loom-project/project.json`
    /// tier (no legacy `.loom/config.json` at all) are discovered through the
    /// resolver. The private/shared defaults tier is disabled for hermeticity.
    #[test]
    #[serial_test::serial]
    fn test_extract_terminal_ids_from_project_tier_only() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join(".loom-project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("project.json"),
            r#"{"terminals": [{"id": "terminal-1"}, {"id": "terminal-2"}]}"#,
        )
        .unwrap();
        // No .loom/config.json exists.
        assert!(!dir.path().join(".loom").join("config.json").exists());

        let result = extract_configured_terminal_ids(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);

        let ids = result.expect("terminals from project tier should be discovered");
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("terminal-1"));
        assert!(ids.contains("terminal-2"));
    }

    /// #4059: an empty `terminals` array in the project tier still yields
    /// `None`, preserving the empty-set-⇒-`None` contract across tiers.
    #[test]
    #[serial_test::serial]
    fn test_extract_terminal_ids_empty_terminals_project_tier() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join(".loom-project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("project.json"), r#"{"terminals": []}"#).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
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
