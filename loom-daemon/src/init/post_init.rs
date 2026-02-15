//! Post-initialization operations
//!
//! Operations performed after file copying: manifest generation and gitignore updates.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Generate installation manifest by running verify-install.sh
///
/// Attempts to run `.loom/scripts/verify-install.sh generate --quiet` to create
/// `.loom/manifest.json` with SHA-256 checksums of all installed files.
/// This is non-fatal - manifest generation failure doesn't prevent installation.
pub fn generate_manifest(workspace_path: &Path) {
    let script = workspace_path
        .join(".loom")
        .join("scripts")
        .join("verify-install.sh");

    if !script.exists() {
        return;
    }

    let result = Command::new("bash")
        .arg(&script)
        .arg("generate")
        .arg("--quiet")
        .current_dir(workspace_path)
        .output();

    match result {
        Ok(output) => {
            if !output.status.success() {
                eprintln!(
                    "Warning: Manifest generation failed (exit {})",
                    output.status.code().unwrap_or(-1)
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: Could not run verify-install.sh: {e}");
        }
    }
}

/// Update .gitignore with Loom ephemeral patterns
///
/// Adds patterns for ephemeral Loom files that shouldn't be committed.
/// Creates .gitignore if it doesn't exist.
pub fn update_gitignore(workspace_path: &Path) -> Result<(), String> {
    let gitignore_path = workspace_path.join(".gitignore");

    // Ephemeral/runtime files that should be ignored.
    // Keep in sync with the Loom source repo's .gitignore (lines 36–78).
    let ephemeral_patterns = [
        ".loom-in-use",
        ".loom-checkpoint",
        ".loom/.daemon.pid",
        ".loom/.daemon.log",
        ".loom/daemon.sock",
        ".loom/daemon-state.json",
        ".loom/daemon-loop.pid",
        ".loom/daemon-metrics.json",
        ".loom/loom-source-path",
        ".loom/[0-9][0-9]-daemon-state.json",
        ".loom/stuck-history.json",
        ".loom/alerts.json",
        ".loom/issue-failures.json",
        ".loom/health-metrics.json",
        ".loom/interventions/",
        ".loom/worktrees/",
        ".loom/state.json",
        ".loom/mcp-command.json",
        ".loom/activity.db",
        ".loom/claims/",
        ".loom/signals/",
        ".loom/status/",
        ".loom/progress/",
        ".loom/diagnostics/",
        ".loom/guide-docs-state.json",
        ".loom/metrics_state.json",
        ".loom/manifest.json",
        ".loom/stuck-config.json",
        ".loom/metrics/",
        ".loom/usage-cache.json",
        ".loom/*.log",
        ".loom/*.sock",
        ".loom/logs/",
    ];

    // Legacy patterns that should be removed during migration.
    // These overly broad patterns block config.json from being tracked.
    let legacy_patterns_to_remove = [".loom/*.json", ".loom/config.json"];

    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

        let mut new_contents = contents.clone();
        let mut modified = false;

        // Remove legacy patterns that block config.json from being tracked.
        // Older installs and /imagine used `.loom/*.json` which is too broad.
        for legacy in &legacy_patterns_to_remove {
            // Remove lines that exactly match the legacy pattern (with optional trailing whitespace)
            let filtered: Vec<&str> = new_contents
                .lines()
                .filter(|line| line.trim() != *legacy)
                .collect();
            let joined = filtered.join("\n");
            // Preserve trailing newline
            let joined = if new_contents.ends_with('\n') && !joined.ends_with('\n') {
                format!("{joined}\n")
            } else {
                joined
            };
            if joined != new_contents {
                new_contents = joined;
                modified = true;
            }
        }

        // Also remove negation patterns that were paired with the legacy *.json glob
        let negation_legacy = "!.loom/roles/*.json";
        let filtered: Vec<&str> = new_contents
            .lines()
            .filter(|line| line.trim() != negation_legacy)
            .collect();
        let joined = filtered.join("\n");
        let joined = if new_contents.ends_with('\n') && !joined.ends_with('\n') {
            format!("{joined}\n")
        } else {
            joined
        };
        if joined != new_contents {
            new_contents = joined;
            modified = true;
        }

        // Add ephemeral patterns if not present
        for pattern in &ephemeral_patterns {
            if !new_contents.contains(pattern) {
                if !new_contents.ends_with('\n') {
                    new_contents.push('\n');
                }
                new_contents.push_str(pattern);
                new_contents.push('\n');
                modified = true;
            }
        }

        // Write back if we made changes
        if modified {
            fs::write(&gitignore_path, new_contents)
                .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
        }
    } else {
        // Create .gitignore with ephemeral patterns
        let mut loom_entries = String::from("# Loom runtime state (don't commit these)\n");
        for pattern in &ephemeral_patterns {
            loom_entries.push_str(pattern);
            loom_entries.push('\n');
        }
        fs::write(&gitignore_path, loom_entries)
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn creates_gitignore_with_all_patterns_when_none_exists() {
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        // Spot-check key runtime patterns
        assert!(contents.contains(".loom-in-use"));
        assert!(contents.contains(".loom-checkpoint"));
        assert!(contents.contains(".loom/daemon-state.json"));
        assert!(contents.contains(".loom/worktrees/"));
        assert!(contents.contains(".loom/progress/"));
        assert!(contents.contains(".loom/*.log"));
        assert!(contents.contains(".loom/logs/"));
        assert!(contents.contains(".loom/daemon-metrics.json"));
        assert!(contents.contains(".loom/[0-9][0-9]-daemon-state.json"));
        assert!(contents.contains(".loom/activity.db"));
        assert!(contents.contains(".loom/issue-failures.json"));
        assert!(contents.contains(".loom/usage-cache.json"));
        assert!(contents.contains("# Loom runtime state"));

        // config.json must NOT be gitignored
        assert!(!contents.contains(".loom/config.json"));
        assert!(!contents.contains(".loom/*.json"));
    }

    #[test]
    fn appends_missing_patterns_to_existing_gitignore() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Pre-existing gitignore with only old patterns
        fs::write(&gitignore, "node_modules/\n.loom/state.json\n.loom/worktrees/\n").unwrap();

        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(&gitignore).unwrap();

        // Original content preserved
        assert!(contents.contains("node_modules/"));
        // Pre-existing patterns not duplicated
        assert_eq!(contents.matches(".loom/state.json").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);
        // New patterns added
        assert!(contents.contains(".loom-in-use"));
        assert!(contents.contains(".loom/daemon-state.json"));
        assert!(contents.contains(".loom/progress/"));
        assert!(contents.contains(".loom/activity.db"));
    }

    #[test]
    fn does_not_duplicate_patterns_on_repeated_runs() {
        let tmp = TempDir::new().unwrap();

        update_gitignore(tmp.path()).unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        assert_eq!(contents.matches(".loom/daemon-state.json").count(), 1);
        assert_eq!(contents.matches(".loom-in-use").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);
    }

    #[test]
    fn covers_all_source_repo_gitignore_patterns() {
        // Verify that every Loom runtime pattern from the source .gitignore
        // is present in the ephemeral_patterns list by running update_gitignore
        // and checking the output.
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        let expected = [
            ".loom-in-use",
            ".loom-checkpoint",
            ".loom/.daemon.pid",
            ".loom/.daemon.log",
            ".loom/daemon.sock",
            ".loom/daemon-state.json",
            ".loom/daemon-loop.pid",
            ".loom/daemon-metrics.json",
            ".loom/loom-source-path",
            ".loom/[0-9][0-9]-daemon-state.json",
            ".loom/stuck-history.json",
            ".loom/alerts.json",
            ".loom/issue-failures.json",
            ".loom/health-metrics.json",
            ".loom/interventions/",
            ".loom/worktrees/",
            ".loom/state.json",
            ".loom/mcp-command.json",
            ".loom/activity.db",
            ".loom/claims/",
            ".loom/signals/",
            ".loom/status/",
            ".loom/progress/",
            ".loom/diagnostics/",
            ".loom/guide-docs-state.json",
            ".loom/metrics_state.json",
            ".loom/manifest.json",
            ".loom/stuck-config.json",
            ".loom/metrics/",
            ".loom/usage-cache.json",
            ".loom/*.log",
            ".loom/*.sock",
            ".loom/logs/",
        ];

        for pattern in &expected {
            assert!(
                contents.contains(pattern),
                "Missing pattern in generated .gitignore: {pattern}"
            );
        }
    }

    #[test]
    fn removes_legacy_broad_json_pattern() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Simulate a gitignore from an older install or /imagine with the legacy patterns
        fs::write(
            &gitignore,
            "node_modules/\n\n# Loom\n.loom/config.json\n.loom/state.json\n.loom/*.json\n!.loom/roles/*.json\n.loom/worktrees/\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(&gitignore).unwrap();

        // Legacy patterns must be removed
        assert!(
            !contents.contains(".loom/*.json"),
            "Legacy .loom/*.json pattern should have been removed"
        );
        assert!(
            !contents.contains(".loom/config.json"),
            "Legacy .loom/config.json pattern should have been removed"
        );
        assert!(
            !contents.contains("!.loom/roles/*.json"),
            "Legacy negation pattern should have been removed"
        );

        // Specific ephemeral patterns should be added instead
        assert!(contents.contains(".loom/daemon-state.json"));
        assert!(contents.contains(".loom/state.json"));
        assert!(contents.contains(".loom/worktrees/"));

        // Non-Loom content preserved
        assert!(contents.contains("node_modules/"));
    }
}
