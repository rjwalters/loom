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

    // Ephemeral files that should be ignored
    let ephemeral_patterns = [
        ".loom/state.json",
        ".loom/worktrees/",
        ".loom/*.log",
        ".loom/*.sock",
    ];

    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

        let mut new_contents = contents.clone();
        let mut modified = false;

        // Add ephemeral patterns if not present
        for pattern in &ephemeral_patterns {
            if !contents.contains(pattern) {
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
        let mut loom_entries = String::from("# Loom - AI Development Orchestration\n");
        for pattern in &ephemeral_patterns {
            loom_entries.push_str(pattern);
            loom_entries.push('\n');
        }
        fs::write(&gitignore_path, loom_entries)
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
    }

    Ok(())
}
