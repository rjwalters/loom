//! Post-initialization operations
//!
//! Operations performed after file copying: manifest generation and gitignore updates.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Gitignore patterns that would shadow installed Loom files like
/// `.loom/scripts/lib/*.sh`. If a user's gitignore contains any of these, the
/// installer must fail loudly: the files will exist on disk after install but
/// will not be committed to the repo, producing a "successful" install that
/// breaks on the next worktree (see issue #3287).
///
/// Note: `.loom/` is intentionally listed even though some users may *want*
/// to ignore the entire `.loom/` directory. In that mode they should not be
/// running `install-loom.sh` at all — the installer's job is to commit Loom
/// files into the target repo. Hard-failing here surfaces that mismatch
/// instead of producing a silently broken install.
const OVERBROAD_LOOM_PATTERNS: &[&str] = &[
    ".loom/",
    ".loom",
    ".loom/*",
    ".loom/**",
    ".loom/scripts/",
    ".loom/scripts",
    ".loom/scripts/*",
    ".loom/scripts/**",
    ".loom/scripts/lib/",
    ".loom/scripts/lib",
    ".loom/scripts/lib/*",
    ".loom/scripts/lib/*.sh",
];

/// Scan `.gitignore` for patterns that would block installed Loom files
/// (specifically `.loom/scripts/lib/*.sh`) from being committed.
///
/// Returns a sorted list of offending pattern lines (trimmed of whitespace).
/// Empty result means the gitignore is safe.
///
/// This is the detection half of the issue #3287 fix. The installer treats a
/// non-empty result as a hard error: the user must remove the broad pattern
/// (or scope it more narrowly) before installation can proceed.
pub fn find_overbroad_loom_patterns(workspace_path: &Path) -> Vec<String> {
    let gitignore_path = workspace_path.join(".gitignore");
    let Ok(contents) = fs::read_to_string(&gitignore_path) else {
        return Vec::new();
    };

    let mut found: Vec<String> = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        // Skip blanks, comments, and negation entries (a negation can't shadow files)
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if OVERBROAD_LOOM_PATTERNS.contains(&line) {
            found.push(line.to_string());
        }
    }
    found.sort();
    found.dedup();
    found
}

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
    //
    // Phase 3.5 (#3402, epic #3372): removed patterns for retired daemon-brain
    // state files (`daemon-state.json`, archived `[0-9][0-9]-daemon-state.json`,
    // `progress/`, `stuck-history.json`, `alerts.json`, `health-metrics.json`)
    // and added `spawn-loop-state.json` for the Phase 1 spawn loop (#3374).
    let ephemeral_patterns = [
        ".loom-in-use",
        ".loom-checkpoint",
        ".loom/.daemon.pid",
        ".loom/.daemon.log",
        ".loom/daemon.sock",
        ".loom/daemon-loop.pid",
        ".loom/daemon-metrics.json",
        ".loom/loom-source-path",
        ".loom/spawn-loop-state.json",
        ".loom/issue-failures.json",
        ".loom/interventions/",
        ".loom/worktrees/",
        ".loom/state.json",
        ".loom/mcp-command.json",
        ".loom/activity.db",
        ".loom/claims/",
        ".loom/signals/",
        ".loom/status/",
        ".loom/retry-state/",
        ".loom/diagnostics/",
        ".loom/guide-docs-state.json",
        ".loom/metrics_state.json",
        ".loom/manifest.json",
        ".loom/stuck-config.json",
        ".loom/metrics/",
        ".loom/usage-cache.json",
        ".loom/claude-config/",
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
        assert!(contents.contains(".loom/spawn-loop-state.json"));
        assert!(contents.contains(".loom/worktrees/"));
        assert!(contents.contains(".loom/*.log"));
        assert!(contents.contains(".loom/logs/"));
        assert!(contents.contains(".loom/daemon-metrics.json"));
        assert!(contents.contains(".loom/activity.db"));
        assert!(contents.contains(".loom/issue-failures.json"));
        assert!(contents.contains(".loom/usage-cache.json"));
        assert!(contents.contains("# Loom runtime state"));

        // Retired daemon-brain patterns must NOT be emitted (Phase 3.5, #3402)
        assert!(!contents.contains(".loom/daemon-state.json"));
        assert!(!contents.contains(".loom/[0-9][0-9]-daemon-state.json"));
        assert!(!contents.contains(".loom/progress/"));
        assert!(!contents.contains(".loom/stuck-history.json"));
        assert!(!contents.contains(".loom/alerts.json"));
        assert!(!contents.contains(".loom/health-metrics.json"));

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
        assert!(contents.contains(".loom/spawn-loop-state.json"));
        assert!(contents.contains(".loom/daemon-metrics.json"));
        assert!(contents.contains(".loom/activity.db"));
    }

    #[test]
    fn does_not_duplicate_patterns_on_repeated_runs() {
        let tmp = TempDir::new().unwrap();

        update_gitignore(tmp.path()).unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        assert_eq!(contents.matches(".loom/spawn-loop-state.json").count(), 1);
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
            ".loom/daemon-loop.pid",
            ".loom/daemon-metrics.json",
            ".loom/loom-source-path",
            ".loom/spawn-loop-state.json",
            ".loom/issue-failures.json",
            ".loom/interventions/",
            ".loom/worktrees/",
            ".loom/state.json",
            ".loom/mcp-command.json",
            ".loom/activity.db",
            ".loom/claims/",
            ".loom/signals/",
            ".loom/status/",
            ".loom/retry-state/",
            ".loom/diagnostics/",
            ".loom/guide-docs-state.json",
            ".loom/metrics_state.json",
            ".loom/manifest.json",
            ".loom/stuck-config.json",
            ".loom/metrics/",
            ".loom/usage-cache.json",
            ".loom/claude-config/",
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

        // Retired patterns (Phase 3.5, #3402) must no longer appear
        let retired = [
            ".loom/daemon-state.json",
            ".loom/[0-9][0-9]-daemon-state.json",
            ".loom/progress/",
            ".loom/stuck-history.json",
            ".loom/alerts.json",
            ".loom/health-metrics.json",
        ];
        for pattern in &retired {
            assert!(
                !contents.contains(pattern),
                "Retired pattern should not be in generated .gitignore: {pattern}"
            );
        }
    }

    #[test]
    fn find_overbroad_loom_patterns_empty_when_no_gitignore() {
        let tmp = TempDir::new().unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert!(
            found.is_empty(),
            "Expected empty result when .gitignore is absent, got {found:?}"
        );
    }

    #[test]
    fn find_overbroad_loom_patterns_detects_dot_loom_slash() {
        // Issue #3287: a target repo with `.loom/` in .gitignore would have all
        // installed Loom files (including `.loom/scripts/lib/*.sh`) silently
        // dropped at commit time.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\n.loom/\nsome-other-pattern\n")
            .unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/".to_string()]);
    }

    #[test]
    fn find_overbroad_loom_patterns_detects_dot_loom_scripts() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), ".loom/scripts/\n# comment\n").unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/scripts/".to_string()]);
    }

    #[test]
    fn find_overbroad_loom_patterns_detects_lib_specifically() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), ".loom/scripts/lib/*.sh\n").unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/scripts/lib/*.sh".to_string()]);
    }

    #[test]
    fn find_overbroad_loom_patterns_ignores_safe_runtime_patterns() {
        // The patterns written by update_gitignore() are runtime-only and
        // must not be flagged as over-broad.
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert!(found.is_empty(), "update_gitignore output should be safe, got: {found:?}");
    }

    #[test]
    fn find_overbroad_loom_patterns_ignores_comments_and_negations() {
        let tmp = TempDir::new().unwrap();
        // Negations and comments mentioning `.loom/` must not be flagged
        fs::write(
            tmp.path().join(".gitignore"),
            "# .loom/ — see docs\n!.loom/scripts/lib/\n.loom/worktrees/\n",
        )
        .unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert!(found.is_empty(), "Expected no flags, got: {found:?}");
    }

    #[test]
    fn find_overbroad_loom_patterns_dedups_and_sorts() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), ".loom/scripts/\n.loom/\n.loom/scripts/\n")
            .unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/".to_string(), ".loom/scripts/".to_string()]);
    }

    #[test]
    fn initialize_workspace_rejects_overbroad_gitignore() {
        // End-to-end: an install against a target with `.loom/` in .gitignore
        // must fail with a clear error message (issue #3287). Without this
        // check, files copy successfully on disk but never make it into the
        // commit, producing a "successful" install that is silently broken.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let defaults = tmp.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults
                .join("scripts")
                .join("lib")
                .join("forge-helpers.sh"),
            "#!/bin/bash",
        )
        .unwrap();

        // Hostile .gitignore: `.loom/` would prevent the lib files from ever
        // being committed (the file copy would succeed, but git would silently
        // drop them at commit time).
        fs::write(workspace.join(".gitignore"), ".loom/\n").unwrap();

        let result = crate::init::initialize_workspace(
            workspace.to_str().unwrap(),
            defaults.to_str().unwrap(),
            false,
        );
        assert!(result.is_err(), "install should refuse hostile .gitignore but returned Ok");
        if let Err(err) = result {
            assert!(
                err.contains(".loom/") && err.contains("Refusing to install"),
                "error message should call out the offending pattern; got: {err}"
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
        assert!(contents.contains(".loom/spawn-loop-state.json"));
        assert!(contents.contains(".loom/state.json"));
        assert!(contents.contains(".loom/worktrees/"));

        // Non-Loom content preserved
        assert!(contents.contains("node_modules/"));
    }
}
