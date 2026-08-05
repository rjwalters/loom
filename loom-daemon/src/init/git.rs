//! Git detection, validation, and path resolution
//!
//! Functions for detecting repository type, validating git structure,
//! and resolving paths relative to git roots.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ValidationReport;

/// Extract repository owner and name from git remote URL
///
/// Parses both HTTPS and SSH git remote URLs to extract owner/repo.
/// Returns None if git remote is not available or URL format is unexpected.
///
/// # Examples
///
/// ```ignore
/// // HTTPS: https://github.com/owner/repo.git -> Some(("owner", "repo"))
/// // SSH: git@github.com:owner/repo.git -> Some(("owner", "repo"))
/// ```
pub fn extract_repo_info(workspace_path: &Path) -> Option<(String, String)> {
    // Get git remote URL
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let remote_url = String::from_utf8(output.stdout).ok()?;
    let remote_url = remote_url.trim();

    // Parse HTTPS URL: https://github.com/owner/repo.git
    if let Some(https_path) = remote_url.strip_prefix("https://github.com/") {
        let path = https_path.strip_suffix(".git").unwrap_or(https_path);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // Parse SSH URL: git@github.com:owner/repo.git
    if let Some(ssh_path) = remote_url.strip_prefix("git@github.com:") {
        let path = ssh_path.strip_suffix(".git").unwrap_or(ssh_path);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

/// Check if the workspace is the Loom source repository itself
///
/// This detects self-installation to prevent overwriting the source of truth
/// with minimal defaults. When detected, initialization switches to validation-only mode.
///
/// # Detection Methods
///
/// 1. **Marker file**: `.loom-source` file exists (most reliable)
/// 2. **Directory structure**: Has `loom-api/`, `loom-daemon/`, and `defaults/` directories
/// 3. **Git remote**: Remote URL matches known Loom repositories
///
/// # Returns
///
/// `true` if this appears to be the Loom source repository
pub fn is_loom_source_repo(workspace_path: &Path) -> bool {
    // Method 1: Check for explicit marker file
    if workspace_path.join(".loom-source").exists() {
        return true;
    }

    // Method 2: Check for Loom-specific directory structure
    let has_loom_api = workspace_path.join("loom-api").is_dir();
    let has_loom_daemon = workspace_path.join("loom-daemon").is_dir();
    let has_defaults = workspace_path.join("defaults").is_dir();

    if has_loom_api && has_loom_daemon && has_defaults {
        // Additional check: defaults should have config.json and roles/
        let defaults_has_config = workspace_path.join("defaults").join("config.json").exists();
        let defaults_has_roles = workspace_path.join("defaults").join("roles").is_dir();

        if defaults_has_config && defaults_has_roles {
            return true;
        }
    }

    // Method 3: Check git remote for known Loom repositories
    if let Some((owner, repo)) = extract_repo_info(workspace_path) {
        // Match various Loom repository locations
        let is_loom_repo =
            (workspace_path.join("loom-api").is_dir() || owner == "rjwalters") && repo == "loom";

        if is_loom_repo {
            return true;
        }
    }

    false
}

/// Collect file stems of every entry in `dir` whose extension matches `ext`.
///
/// Returns an empty vec when the directory is unreadable or missing — callers
/// surface the "missing directory" issue separately.
fn collect_file_stems(dir: &Path, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == ext) {
            if let Some(name) = path.file_stem() {
                out.push(name.to_string_lossy().to_string());
            }
        }
    }
    out
}

/// Scan `dir` for files with extension `ext`, populating `found`. If `dir`
/// does not exist, push a `Missing <missing_msg>` entry into `issues`.
fn scan_or_record_missing(
    dir: &Path,
    ext: &str,
    found: &mut Vec<String>,
    issues: &mut Vec<String>,
    missing_msg: &str,
) {
    if dir.is_dir() {
        found.extend(collect_file_stems(dir, ext));
    } else {
        issues.push(format!("Missing {missing_msg}"));
    }
}

/// Validate an existing Loom source repository configuration
///
/// Instead of copying files, this validates that the expected structure exists
/// and reports any issues found.
pub fn validate_loom_source_repo(workspace_path: &Path) -> ValidationReport {
    let mut report = ValidationReport::default();
    let loom_path = workspace_path.join(".loom");

    scan_or_record_missing(
        &loom_path.join("roles"),
        "md",
        &mut report.roles_found,
        &mut report.issues,
        ".loom/roles/ directory",
    );

    scan_or_record_missing(
        &loom_path.join("scripts"),
        "sh",
        &mut report.scripts_found,
        &mut report.issues,
        ".loom/scripts/ directory",
    );

    scan_or_record_missing(
        &workspace_path.join(".claude").join("commands").join("loom"),
        "md",
        &mut report.commands_found,
        &mut report.issues,
        ".claude/commands/loom/ directory",
    );

    // .claude/agents/ holds subagent definitions (loom-builder, loom-judge, …).
    // Required for native Claude Code `subagent_type` dispatch — without these
    // files fresh installs cannot use the loom-* subagents. See issue #3310.
    scan_or_record_missing(
        &workspace_path.join(".claude").join("agents"),
        "md",
        &mut report.agents_found,
        &mut report.issues,
        ".claude/agents/ directory",
    );

    // Check documentation files
    report.has_claude_md = workspace_path.join("CLAUDE.md").exists();
    if !report.has_claude_md {
        report.issues.push("Missing CLAUDE.md".to_string());
    }

    // Check AGENTS.md (issue #4479, dual-runtime instruction anchor).
    // Intentionally NOT mandatory: unlike CLAUDE.md, pre-existing installs from
    // before this feature landed won't have AGENTS.md, and that must not be
    // flagged as a validation issue for those repos.
    report.has_agents_md = workspace_path.join("AGENTS.md").exists();

    // Check labels.yml
    report.has_labels_yml = workspace_path.join(".github").join("labels.yml").exists();
    if !report.has_labels_yml {
        report.issues.push("Missing .github/labels.yml".to_string());
    }

    // Validate minimum expected counts
    if report.roles_found.len() < 5 {
        report.issues.push(format!(
            "Expected at least 5 role definitions, found {}",
            report.roles_found.len()
        ));
    }

    if report.scripts_found.len() < 2 {
        report
            .issues
            .push(format!("Expected at least 2 scripts, found {}", report.scripts_found.len()));
    }

    if report.commands_found.len() < 4 {
        report.issues.push(format!(
            "Expected at least 4 slash commands, found {}",
            report.commands_found.len()
        ));
    }

    // The shipped agents pool currently contains 11 loom-* subagents
    // (architect, auditor, builder, champion, curator, daemon, doctor,
    // guide, hermit, judge, shepherd). Require at least 5 so a degraded
    // install (missing core dispatchers like loom-builder) is flagged.
    if report.agents_found.len() < 5 {
        report.issues.push(format!(
            "Expected at least 5 subagent definitions in .claude/agents/, found {}",
            report.agents_found.len()
        ));
    }

    report
}

/// Validate that a path is a git repository
///
/// Checks that the path exists, is a directory, and contains a .git directory.
pub fn validate_git_repository(path: &str) -> Result<(), String> {
    let workspace_path = Path::new(path);

    // Check if the path exists
    if !workspace_path.exists() {
        return Err(format!("Path does not exist: {path}"));
    }

    // Check if it's a directory
    if !workspace_path.is_dir() {
        return Err(format!("Path is not a directory: {path}"));
    }

    // Check for .git directory
    let git_path = workspace_path.join(".git");
    if !git_path.exists() {
        return Err(format!("Not a git repository (no .git directory found): {path}"));
    }

    Ok(())
}

/// Helper function to find git repository root by searching for .git directory
pub fn find_git_root() -> Option<PathBuf> {
    // Start from current directory
    let mut current = std::env::current_dir().ok()?;

    loop {
        let git_dir = current.join(".git");

        // Security: Check if .git exists and is NOT a symlink
        // Prevents symlink-based directory traversal attacks (CWE-59)
        if git_dir.exists() {
            if let Ok(metadata) = git_dir.symlink_metadata() {
                if metadata.is_symlink() {
                    // Reject symlinks to prevent directory escape
                    return None;
                }
            }
            return Some(current);
        }

        // Move up to parent directory
        if !current.pop() {
            // Reached filesystem root without finding .git
            return None;
        }
    }
}

/// Env var overriding where `loom-daemon` looks for its machine-level,
/// standalone-install `defaults/` payload (see [`machine_level_defaults_path`]).
/// Set to a non-empty path to point at an alternate location; set to the
/// empty string to disable this search strategy entirely.
pub const MACHINE_DEFAULTS_ENV: &str = "LOOM_DAEMON_DEFAULTS_DIR";

/// Home-relative default location `scripts/install/provision-daemon.sh`
/// mirrors its `defaults/` payload to for a standalone (no on-host `loom`
/// git checkout) install — see `provision_machine_daemon`'s third argument.
/// Deliberately distinct from `~/.local/share/loom` (the FULL machine
/// checkout `provision_loom_dispatcher` symlinks there), so this narrower
/// payload copy can never collide with, or get shadowed by, that symlink
/// management.
const MACHINE_DEFAULTS_REL: &str = ".local/share/loom-daemon/defaults";

/// Resolve the machine-level standalone-install defaults payload path
/// (Issue #5389): [`MACHINE_DEFAULTS_ENV`] if set to a non-empty value, else
/// `~/` + [`MACHINE_DEFAULTS_REL`]. Returns `None` when the env var is
/// explicitly set to an empty string (strategy disabled) or when no home
/// directory can be determined.
fn machine_level_defaults_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(MACHINE_DEFAULTS_ENV) {
        return if p.is_empty() {
            None
        } else {
            Some(PathBuf::from(p))
        };
    }
    dirs::home_dir().map(|h| h.join(MACHINE_DEFAULTS_REL))
}

/// Resolve defaults directory path
///
/// Tries development path first, then falls back to bundled resource path.
/// This handles both development mode and production builds.
///
/// # Search Order
///
/// 1. Provided path (development mode - relative to cwd)
/// 2. Git repository root + path (handles git worktrees)
/// 3. Machine-level standalone-install payload (`$LOOM_DAEMON_DEFAULTS_DIR`,
///    else `~/.local/share/loom-daemon/defaults` — mirrored there by
///    `scripts/install/provision-daemon.sh` at install/update time for a
///    `loom-daemon` installed with no on-host `loom` git checkout, Issue
///    #5389). Independent of cwd, so this works no matter where
///    `loom-daemon init` is invoked from.
/// 4. Bundled resource path (production mode - .app/Contents/Resources/)
pub fn resolve_defaults_path(defaults_path: &str) -> Result<PathBuf, String> {
    let mut tried_paths = Vec::new();

    // Try the provided path first (development mode - relative to cwd)
    let dev_path = PathBuf::from(defaults_path);
    tried_paths.push(dev_path.display().to_string());
    if dev_path.exists() {
        return Ok(dev_path);
    }

    // Try finding defaults relative to git repository root
    // This handles the case where we're running from a git worktree
    if let Some(git_root) = find_git_root() {
        let git_root_defaults = git_root.join(defaults_path);
        tried_paths.push(git_root_defaults.display().to_string());
        if git_root_defaults.exists() {
            return Ok(git_root_defaults);
        }
    }

    // Machine-level standalone-install payload (#5389): a `loom-daemon`
    // installed with no on-host `loom` git checkout (e.g. a lean worker
    // image that only ships `~/.local/bin/loom-daemon`) has no cwd- or
    // git-root-relative `defaults/` to find. `provision-daemon.sh` mirrors
    // its own `defaults/` payload to this well-known location at
    // provision/update time; check it here independent of cwd.
    if let Some(machine_defaults) = machine_level_defaults_path() {
        tried_paths.push(machine_defaults.display().to_string());
        if machine_defaults.exists() {
            return Ok(machine_defaults);
        }
    }

    // Legacy: try resolving as bundled resource (when shipped inside a macOS .app bundle).
    // The GUI .app shipping path was removed in v0.9; this fallback is kept defensively for
    // any third-party packagers that wrap loom-daemon in a Contents/MacOS layout.
    if let Ok(exe_path) = std::env::current_exe() {
        // Get the app bundle Resources directory
        if let Some(exe_dir) = exe_path.parent() {
            // exe is in Contents/MacOS/, resources are in Contents/Resources/
            if let Some(contents_dir) = exe_dir.parent() {
                let resources_dir = contents_dir.join("Resources");

                // Try with _up_ prefix (legacy bundling layout used `_up_/defaults`)
                let up_path = resources_dir.join("_up_").join(defaults_path);
                tried_paths.push(up_path.display().to_string());
                if up_path.exists() {
                    return Ok(up_path);
                }

                // Try with subdirectory name (standard .app bundling layout)
                let resources_path = resources_dir.join(defaults_path);
                tried_paths.push(resources_path.display().to_string());
                if resources_path.exists() {
                    return Ok(resources_path);
                }

                // Try the Resources directory itself (in case bundling flattens structure)
                tried_paths.push(resources_dir.display().to_string());
                if resources_dir.join("config.json").exists() {
                    return Ok(resources_dir);
                }
            }
        }
    }

    Err(format!(
        "Defaults directory not found. Tried paths:\n  {}",
        tried_paths.join("\n  ")
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // std::env::set_var mutates process-global state; serialize the tests
    // below that touch MACHINE_DEFAULTS_ENV so parallel execution doesn't
    // race on it. Module-local lock is sufficient here (unlike
    // worktree_root.rs's ENV_LOCK) because no other module reads
    // LOOM_DAEMON_DEFAULTS_DIR.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with `LOOM_DAEMON_DEFAULTS_DIR` set to `value` (or unset if
    /// `None`), restoring the prior value afterward. Serialized via
    /// `ENV_LOCK`.
    fn with_machine_defaults_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var(MACHINE_DEFAULTS_ENV).ok();
        match value {
            Some(v) => std::env::set_var(MACHINE_DEFAULTS_ENV, v),
            None => std::env::remove_var(MACHINE_DEFAULTS_ENV),
        }
        let result = f();
        match prev {
            Some(p) => std::env::set_var(MACHINE_DEFAULTS_ENV, p),
            None => std::env::remove_var(MACHINE_DEFAULTS_ENV),
        }
        result
    }

    #[test]
    fn test_machine_level_defaults_path_honors_env_override() {
        with_machine_defaults_env(Some("/tmp/some/custom/defaults"), || {
            let resolved = machine_level_defaults_path();
            assert_eq!(resolved, Some(PathBuf::from("/tmp/some/custom/defaults")));
        });
    }

    #[test]
    fn test_machine_level_defaults_path_empty_env_disables() {
        with_machine_defaults_env(Some(""), || {
            let resolved = machine_level_defaults_path();
            assert_eq!(resolved, None);
        });
    }

    #[test]
    fn test_machine_level_defaults_path_default_is_home_relative() {
        with_machine_defaults_env(None, || {
            let resolved = machine_level_defaults_path();
            if let Some(home) = dirs::home_dir() {
                assert_eq!(resolved, Some(home.join(".local/share/loom-daemon/defaults")));
            } else {
                // No resolvable home directory on this host (unusual, e.g. a
                // minimal CI container) — the function correctly returns None.
                assert_eq!(resolved, None);
            }
        });
    }

    #[test]
    fn test_resolve_defaults_path_falls_back_to_machine_level_payload() {
        // A defaults_path guaranteed absent both as a cwd-relative path and
        // relative to this crate's own git root, so strategies 1 and 2 both
        // miss and the machine-level strategy (3) is exercised.
        let bogus_defaults_path = "definitely-does-not-exist-issue-5389-defaults";

        let payload_dir = TempDir::new().unwrap();
        // Give the payload directory recognizable contents so a future
        // regression (e.g. accidentally returning the parent dir) is caught.
        fs::write(payload_dir.path().join("config.json"), "{}").unwrap();

        with_machine_defaults_env(Some(payload_dir.path().to_str().unwrap()), || {
            let resolved = resolve_defaults_path(bogus_defaults_path).unwrap();
            assert_eq!(resolved, payload_dir.path());
        });
    }

    #[test]
    fn test_resolve_defaults_path_errors_when_machine_level_payload_missing() {
        let bogus_defaults_path = "definitely-does-not-exist-issue-5389-defaults";

        // Point the machine-level candidate at a path that does not exist,
        // so ALL search strategies miss and resolve_defaults_path must
        // return an error (not silently succeed) — the load-bearing
        // assertion for the "exits non-zero, doesn't look like success"
        // half of #5389.
        let missing = TempDir::new().unwrap().path().join("nope");

        with_machine_defaults_env(Some(missing.to_str().unwrap()), || {
            let result = resolve_defaults_path(bogus_defaults_path);
            assert!(result.is_err());
            let err = result.unwrap_err();
            // The tried-paths list should include the machine-level
            // candidate so an operator can see it was considered.
            assert!(err.contains(missing.to_str().unwrap()));
        });
    }

    #[test]
    fn test_validate_git_repository() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Not a git repo yet
        let result = validate_git_repository(workspace.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not a git repository"));

        // Create .git directory
        fs::create_dir(workspace.join(".git")).unwrap();

        // Should now be valid
        let result = validate_git_repository(workspace.to_str().unwrap());
        assert!(result.is_ok());
    }
}
