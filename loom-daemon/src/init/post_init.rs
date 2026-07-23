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

/// Sentinel that opens the Loom-managed `.gitignore` block.
///
/// The managed block is delimited by [`GITIGNORE_BEGIN_MARKER`] and
/// [`GITIGNORE_END_MARKER`] so that both `update_gitignore` (add/refresh) and
/// `scripts/uninstall-loom.sh` (remove) operate on a single, self-locating,
/// contiguous region. This mirrors the `<!-- BEGIN/END LOOM ORCHESTRATION -->`
/// convention already used for CLAUDE.md. See issue #3590.
pub const GITIGNORE_BEGIN_MARKER: &str = "# >>> loom-managed (do not edit) >>>";

/// Sentinel that closes the Loom-managed `.gitignore` block.
pub const GITIGNORE_END_MARKER: &str = "# <<< loom-managed <<<";

/// Human-readable header emitted inside the managed block. Pre-#3590 installs
/// wrote this same line as a bare (markerless) header; detecting it lets us
/// migrate those installs to the marked form in place.
const GITIGNORE_BLOCK_HEADER: &str = "# Loom runtime state (don't commit these)";

/// Ephemeral/runtime files that should be ignored — the single source of truth
/// for the Loom-managed `.gitignore` block.
///
/// Keep in sync with the Loom source repo's `.gitignore`.
///
/// Phase 3.5 (#3402, epic #3372): removed patterns for retired daemon-brain
/// state files (`daemon-state.json`, archived `[0-9][0-9]-daemon-state.json`,
/// `progress/`, `stuck-history.json`, `alerts.json`, `health-metrics.json`)
/// and added `spawn-loop-state.json` for the Phase 1 spawn loop (#3374).
///
/// #3778: closed the drift where this installer-managed list had fallen behind
/// the source `.gitignore`, so a consumer repo's re-synced loom-managed block
/// still omitted several Loom-owned transient paths — added `.loom-managed`,
/// `.loom/exit-codes/`, `.loom/sweep-run/`, `.loom/stats/`, `.loom/spawn-loop.pid`,
/// and `.loom/stop-spawn-loop`. The marker-delimited block is refreshed in place
/// on every `update_gitignore`, so re-running the installer re-syncs consumers.
pub const EPHEMERAL_PATTERNS: &[&str] = &[
    ".loom-in-use",
    ".loom-checkpoint",
    // Worktree sentinel dropped by worktree.sh into each issue worktree; must be
    // ignored so a builder's `git add -A` doesn't sweep it into a commit (#3778).
    ".loom-managed",
    ".loom/.daemon.pid",
    ".loom/.daemon.log",
    ".loom/daemon.sock",
    ".loom/daemon-loop.pid",
    ".loom/daemon-metrics.json",
    ".loom/loom-source-path",
    ".loom/spawn-loop-state.json",
    ".loom/spawn-loop.pid",
    ".loom/stop-spawn-loop",
    ".loom/issue-failures.json",
    ".loom/interventions/",
    ".loom/worktrees/",
    ".loom/state.json",
    ".loom/mcp-command.json",
    ".loom/activity.db",
    ".loom/claims/",
    ".loom/locks/",
    ".loom/signals/",
    ".loom/status/",
    ".loom/retry-state/",
    ".loom/exit-codes/",
    ".loom/sweep-checkpoint/",
    ".loom/sweep-run/",
    ".loom/stats/",
    ".loom/diagnostics/",
    ".loom/guide-docs-state.json",
    ".loom/metrics_state.json",
    ".loom/manifest.json",
    ".loom/stuck-config.json",
    ".loom/metrics/",
    ".loom/usage-cache.json",
    ".loom/claude-config/",
    // Secret-bearing token pool + repo-local account source (#3695). These
    // hold OAuth keys and must never be committed.
    ".loom/tokens/",
    ".loom/accounts.env",
    // Uncommitted canary confirmation sentinel (#3731). Its guardrail power comes
    // from being uncommitted, so it must never be tracked.
    ".loom/CANARY",
    ".loom/*.log",
    ".loom/*.sock",
    ".loom/logs/",
];

/// Build the Loom-managed `.gitignore` block (marker lines + header + patterns),
/// with no leading or trailing newline. Callers add surrounding newlines.
fn managed_gitignore_block() -> String {
    let mut block = String::new();
    block.push_str(GITIGNORE_BEGIN_MARKER);
    block.push('\n');
    block.push_str(GITIGNORE_BLOCK_HEADER);
    block.push('\n');
    for pattern in EPHEMERAL_PATTERNS {
        block.push_str(pattern);
        block.push('\n');
    }
    block.push_str(GITIGNORE_END_MARKER);
    block
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
    let block = managed_gitignore_block();

    // Create a fresh .gitignore containing only the managed block.
    if !gitignore_path.exists() {
        fs::write(&gitignore_path, format!("{block}\n"))
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
        return Ok(());
    }

    let contents = fs::read_to_string(&gitignore_path)
        .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

    // Split on '\n'. When the file ends with '\n', the trailing element is an
    // empty string; joining back with '\n' reproduces the original bytes
    // exactly, so line-vector edits are byte-preserving for untouched regions.
    let mut lines: Vec<String> = contents.split('\n').map(str::to_string).collect();

    // Remove legacy over-broad patterns that block config.json from being
    // tracked (older installs and /imagine used `.loom/*.json`), plus the
    // negation that was paired with that glob.
    let legacy_overbroad = [".loom/*.json", ".loom/config.json", "!.loom/roles/*.json"];
    lines.retain(|line| !legacy_overbroad.contains(&line.trim()));

    let begin = lines
        .iter()
        .position(|l| l.trim() == GITIGNORE_BEGIN_MARKER);
    let end = lines.iter().position(|l| l.trim() == GITIGNORE_END_MARKER);

    let block_lines: Vec<String> = block.split('\n').map(str::to_string).collect();

    match (begin, end) {
        (Some(b), Some(e)) if b <= e => {
            // Marked block already present: refresh it in place (patterns may
            // have changed between versions) without moving it.
            lines.splice(b..=e, block_lines.iter().cloned());
        }
        _ => {
            // No well-formed BEGIN <= END pair. This covers a legacy markerless
            // block, a fresh consumer file, and corrupted single/misordered
            // markers. Migrate in place rather than relocating to EOF (#3592).

            // 1. Normalize orphan/corrupted markers. Because we did not match the
            //    in-place arm, any marker line present here is stray (an END-only
            //    orphan, a BEGIN-only orphan, or an END-before-BEGIN pair). Drop
            //    them so they can neither accumulate every run (orphan-END
            //    unbounded growth) nor drive a destructive splice that swallows
            //    user content (orphan-BEGIN). #3592
            lines.retain(|line| {
                let t = line.trim();
                t != GITIGNORE_BEGIN_MARKER && t != GITIGNORE_END_MARKER
            });

            // 2. Locate the legacy markerless block: the bare header and/or bare
            //    ephemeral pattern lines. Remember where the first such line sits
            //    so the marked block can be spliced back into that same span.
            let is_legacy = |line: &str| {
                let t = line.trim();
                t == GITIGNORE_BLOCK_HEADER || EPHEMERAL_PATTERNS.contains(&t)
            };
            let first_legacy = lines.iter().position(|l| is_legacy(l));

            match first_legacy {
                Some(start) => {
                    // Remove every legacy line, then splice the marked block into
                    // the original position. Because we replace in place instead
                    // of removing-then-appending, flanking blank lines stay put:
                    // the block does not relocate to EOF and no double-blank
                    // artifact is left behind where the old block used to sit
                    // (#3592). Any user content that followed the legacy block
                    // still follows the managed block.
                    lines.retain(|l| !is_legacy(l));
                    lines.splice(start..start, block_lines.iter().cloned());
                }
                None => {
                    // Genuinely no Loom header/patterns/markers: create the block
                    // at EOF. Drop exactly one trailing empty element (the file's
                    // final '\n') so the block is appended directly after the
                    // existing content with no spurious blank line. This makes the
                    // append the exact inverse of uninstall's marker-span
                    // deletion, so an install -> uninstall -> install round-trip
                    // is byte-identical (issue #3590).
                    if lines.last().is_some_and(String::is_empty) {
                        lines.pop();
                    }
                    for bl in &block_lines {
                        lines.push(bl.clone());
                    }
                    // Re-add exactly one trailing empty element => single '\n'.
                    lines.push(String::new());
                }
            }
        }
    }

    let new_contents = lines.join("\n");
    if new_contents != contents {
        fs::write(&gitignore_path, &new_contents)
            .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
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
        // Runtime dirs added in #3635 — must appear exactly once
        assert_eq!(contents.matches(".loom/sweep-checkpoint/").count(), 1);
        assert_eq!(contents.matches(".loom/locks/").count(), 1);
        assert!(contents.contains("# Loom runtime state"));

        // #3778: patterns that had drifted out of this installer-managed list
        // relative to the source .gitignore — a consumer re-sync must now emit
        // them so Loom-owned transient state never surfaces as untracked dirt.
        assert!(contents.contains(".loom-managed"));
        assert!(contents.contains(".loom/exit-codes/"));
        assert!(contents.contains(".loom/sweep-run/"));
        assert!(contents.contains(".loom/stats/"));
        assert!(contents.contains(".loom/spawn-loop.pid"));
        assert!(contents.contains(".loom/stop-spawn-loop"));

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
        assert_eq!(contents.matches(".loom/sweep-checkpoint/").count(), 1);
        assert_eq!(contents.matches(".loom/locks/").count(), 1);
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
            ".loom/locks/",
            ".loom/signals/",
            ".loom/status/",
            ".loom/retry-state/",
            ".loom/sweep-checkpoint/",
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

    /// Mimic `scripts/uninstall-loom.sh`'s marker-span deletion: drop every
    /// line from the BEGIN marker through the END marker (inclusive).
    fn remove_managed_block(contents: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        let mut in_block = false;
        for line in contents.split('\n') {
            if line.trim() == GITIGNORE_BEGIN_MARKER {
                in_block = true;
                continue;
            }
            if in_block {
                if line.trim() == GITIGNORE_END_MARKER {
                    in_block = false;
                }
                continue;
            }
            out.push(line);
        }
        out.join("\n")
    }

    #[test]
    fn create_wraps_patterns_in_markers() {
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        assert!(contents.contains(GITIGNORE_BEGIN_MARKER), "missing BEGIN marker");
        assert!(contents.contains(GITIGNORE_END_MARKER), "missing END marker");
        // BEGIN must precede END, and each appears exactly once.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        let b = contents.find(GITIGNORE_BEGIN_MARKER).unwrap();
        let e = contents.find(GITIGNORE_END_MARKER).unwrap();
        assert!(b < e, "BEGIN marker must precede END marker");
        // Patterns live inside the block.
        assert!(contents.contains(".loom/worktrees/"));
    }

    #[test]
    fn in_place_update_does_not_move_or_duplicate_block() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // User content that already carries a marked block, followed by more
        // user content AFTER the block (so an EOF re-append would reorder it).
        fs::write(
            &gitignore,
            format!(
                "node_modules/\n{block}\n# trailing user content\ndist/\n",
                block = managed_gitignore_block()
            ),
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // Exactly one block, refreshed in place.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        // Content after the block is untouched and still after the block.
        let end_pos = contents.find(GITIGNORE_END_MARKER).unwrap();
        let trailing_pos = contents.find("# trailing user content").unwrap();
        assert!(trailing_pos > end_pos, "trailing user content must not move");
        assert!(contents.contains("node_modules/"));
        assert!(contents.contains("dist/"));
    }

    #[test]
    fn roundtrip_is_byte_identical() {
        // install -> uninstall -> install on a committed, customized .gitignore
        // (whose Loom block is exactly what install produces) must be a no-op at
        // the byte level. This is the core acceptance criterion of issue #3590.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // A realistic consumer .gitignore.
        fs::write(&gitignore, "# consumer rules\nnode_modules/\n__pycache__/\n.venv/\n*.log\n")
            .unwrap();

        // First install writes the managed block.
        update_gitignore(tmp.path()).unwrap();
        let after_install = fs::read_to_string(&gitignore).unwrap();
        assert!(after_install.contains(GITIGNORE_BEGIN_MARKER));

        // Simulate `uninstall-loom.sh` removing the marked span, then reinstall.
        let after_uninstall = remove_managed_block(&after_install);
        fs::write(&gitignore, &after_uninstall).unwrap();
        update_gitignore(tmp.path()).unwrap();
        let after_reinstall = fs::read_to_string(&gitignore).unwrap();

        assert_eq!(
            after_install, after_reinstall,
            "install -> uninstall -> install must be byte-identical"
        );

        // And uninstall fully removes the Loom block (no residual patterns).
        assert!(!after_uninstall.contains(GITIGNORE_BEGIN_MARKER));
        assert!(!after_uninstall.contains(GITIGNORE_END_MARKER));
        assert!(!after_uninstall.contains(".loom/worktrees/"));
        assert!(!after_uninstall.contains("# Loom runtime state"));
        // User content survives the uninstall verbatim.
        assert_eq!(
            after_uninstall,
            "# consumer rules\nnode_modules/\n__pycache__/\n.venv/\n*.log\n"
        );
    }

    #[test]
    fn migrates_legacy_markerless_block_in_place() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Pre-#3590 install: bare header + bare patterns, no markers.
        fs::write(
            &gitignore,
            "node_modules/\n# Loom runtime state (don't commit these)\n\
             .loom/state.json\n.loom/worktrees/\n.loom/*.log\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // Migrated to the marked form.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        // No duplicate bare patterns outside the block.
        assert_eq!(contents.matches(".loom/state.json").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);
        // Now idempotent: a second run is a no-op.
        update_gitignore(tmp.path()).unwrap();
        let again = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(contents, again, "post-migration update must be idempotent");
    }

    #[test]
    fn migrates_mid_file_legacy_block_in_place_without_double_blank() {
        // Regression for #3592: a legacy markerless block sitting mid-file,
        // flanked by a blank line on each side with user content AFTER it, must
        // migrate to the marked form (a) in place (not relocated to EOF),
        // (b) without leaving a double-blank artifact, and (c) idempotently.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(
            &gitignore,
            "node_modules/\n\n# Loom runtime state (don't commit these)\n\
             .loom-in-use\n.loom/state.json\n.loom/worktrees/\n.loom/logs/\n\
             \ndist/\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // (a) Exactly one BEGIN and one END marker.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);

        // (b) The block stayed in place: user content that followed the legacy
        // block still follows the managed block, and content that preceded it
        // still precedes it.
        let begin_pos = contents.find(GITIGNORE_BEGIN_MARKER).unwrap();
        let end_pos = contents.find(GITIGNORE_END_MARKER).unwrap();
        let node_pos = contents.find("node_modules/").unwrap();
        let dist_pos = contents.find("dist/").unwrap();
        assert!(node_pos < begin_pos, "node_modules/ must stay before the block");
        assert!(dist_pos > end_pos, "dist/ must stay after the block");

        // (c) No double-blank artifact anywhere.
        assert!(
            !contents.contains("\n\n\n"),
            "migration must not leave a double-blank artifact: {contents:?}"
        );

        // No duplicate patterns outside the block.
        assert_eq!(contents.matches(".loom/state.json").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);

        // (d) A second run is a byte-identical no-op.
        update_gitignore(tmp.path()).unwrap();
        let again = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(contents, again, "second migration run must be a byte-identical no-op");
    }

    #[test]
    fn orphan_end_marker_converges_without_growth() {
        // Regression for #3592: a stray END marker above user content (a
        // corrupted / hand-edited .gitignore) previously drove unbounded marker
        // growth (+1 block per run) because `position()` found the orphan END,
        // `begin > end`, and the legacy arm re-appended a fresh block forever.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(&gitignore, "# <<< loom-managed <<<\nnode_modules/\ndist/\n").unwrap();

        update_gitignore(tmp.path()).unwrap();
        let after_one = fs::read_to_string(&gitignore).unwrap();

        // Converges to exactly one well-formed marked block.
        assert_eq!(after_one.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(after_one.matches(GITIGNORE_END_MARKER).count(), 1);
        // User content survives.
        assert!(after_one.contains("node_modules/"));
        assert!(after_one.contains("dist/"));

        // A second run is a byte-identical no-op (no growth).
        update_gitignore(tmp.path()).unwrap();
        let after_two = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(after_one, after_two, "orphan-END migration must converge, not grow");
        assert_eq!(after_two.matches(GITIGNORE_END_MARKER).count(), 1);
    }

    #[test]
    fn orphan_begin_marker_converges_without_eating_user_content() {
        // Regression for #3592: a stray BEGIN marker above user content
        // previously converged only by having the in-place splice swallow every
        // line from the orphan BEGIN through the real END, silently deleting the
        // intervening user line (e.g. `dist/`).
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(&gitignore, "# >>> loom-managed (do not edit) >>>\nnode_modules/\ndist/\n")
            .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let after_one = fs::read_to_string(&gitignore).unwrap();

        // Converges to exactly one well-formed marked block.
        assert_eq!(after_one.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(after_one.matches(GITIGNORE_END_MARKER).count(), 1);
        // The user line between the orphan marker and EOF must survive.
        assert!(after_one.contains("dist/"), "user content must not be eaten");
        assert!(after_one.contains("node_modules/"));

        // A second run is a byte-identical no-op.
        update_gitignore(tmp.path()).unwrap();
        let after_two = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(after_one, after_two, "orphan-BEGIN migration must be idempotent");
    }
}
