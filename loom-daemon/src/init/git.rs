//! Git detection, validation, and path resolution
//!
//! Functions for detecting repository type, validating git structure,
//! and resolving paths relative to git roots.

use std::collections::HashSet;
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

// ============================================================================
// Dogfood symlink creation (Issue #6440)
// ============================================================================
//
// The loom source repo dogfoods its own `.claude/commands/loom/` and
// `.claude/agents/` from `defaults/.claude/...` via relative symlinks — the
// same content every OTHER repo receives as tracked, installed real files.
// Historically these symlinks were only ever created by the interactive shell
// installer (`scripts/install/dogfood-commands.sh`'s `link_dogfood_commands`,
// and the `.claude/agents` linker inlined in `scripts/install-loom.sh`).
// `loom-daemon init` — the path `fleet add-worker` actually calls when
// provisioning a fresh daemon workspace — only ever *validated* that these
// paths existed (`validate_loom_source_repo` above) and reported them
// missing; it never created them. A worker provisioned by any route other
// than the interactive installer (e.g. `fleet add-worker` cloning straight
// into `~/loom-workspaces/`) therefore silently refused every dispatch with
// "workspace is missing .claude/commands/loom/sweep.md" until a human
// manually ran the shell installer or hand-created the symlinks — undetected
// for two weeks on one fleet host. `link_dogfood_symlinks` below is the Rust
// port of both shell code paths, called from `initialize_workspace`'s
// self-install branch so `loom-daemon init` closes this gap itself.

/// Files present under `dir` but not under `reference` (relative paths,
/// recursive). Mirrors the shell installer's `comm -23 <(find dir) <(find
/// reference)` local-only-file check, run before replacing a real directory
/// with a symlink so genuinely local content is never silently discarded.
///
/// Fails safe: an unreadable `dir` or `reference` contributes nothing to the
/// respective side rather than erroring, so a transient read failure never
/// blocks the common (nothing local-only) case — mirroring the shell
/// implementation's `2>/dev/null || true`.
fn local_only_files(dir: &Path, reference: &Path) -> Vec<String> {
    fn collect(base: &Path, cur: &Path, out: &mut HashSet<String>) {
        let Ok(entries) = fs::read_dir(cur) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                collect(base, &path, out);
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.insert(rel.to_string_lossy().to_string());
            }
        }
    }

    if !dir.is_dir() {
        return Vec::new();
    }
    let mut dir_files = HashSet::new();
    collect(dir, dir, &mut dir_files);
    let mut reference_files = HashSet::new();
    collect(reference, reference, &mut reference_files);

    let mut extra: Vec<String> = dir_files.difference(&reference_files).cloned().collect();
    extra.sort();
    extra
}

/// Idempotently establish `link_path` as a relative symlink pointing at
/// `relative_target` (resolved from `link_path`'s own parent directory, e.g.
/// `../../defaults/.claude/commands/loom`), mirroring the idempotency rules
/// `link_dogfood_commands` (shell) already implements — unified into one
/// helper since both the `.claude/commands/loom` and `.claude/agents` cases
/// follow the identical decision tree, only the paths differ:
///
/// - `source_abs` missing -> soft skip; nothing to link yet, not an error.
/// - `link_path` missing -> create the symlink.
/// - `link_path` already a symlink whose *raw* target string (not resolved)
///   equals `relative_target` -> no-op.
/// - `link_path` a symlink to something else (drifted / stale) -> replace.
/// - `link_path` a real directory whose content is a subset of `source_abs`
///   (including empty) -> replace with the symlink; safe because
///   `defaults/` already holds every file it contains.
/// - `link_path` a real directory containing files not present under
///   `source_abs` -> soft skip; refuse to discard local-only content (never
///   silently corrupt a workspace — see issue #6440's complexity note).
/// - `link_path` a plain (non-directory) file -> soft skip; never touched.
///
/// Every branch above is advisory-only: this never returns an `Err` the
/// caller must propagate, matching the shell version's `return 0` on every
/// skip path — a fresh-clone `loom-daemon init` must never fail just because
/// dogfood-symlink creation hit a soft-skip case.
fn ensure_dir_symlink(link_path: &Path, relative_target: &str, source_abs: &Path) -> String {
    let link_display = link_path.display().to_string();

    if !source_abs.is_dir() {
        return format!(
            "Skipped {link_display} symlink: source {} does not exist",
            source_abs.display()
        );
    }

    if let Some(parent) = link_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return format!("Failed to create {}: {e}", parent.display());
        }
    }

    match fs::symlink_metadata(link_path) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(link_path) {
            Ok(existing) if existing.to_string_lossy() == relative_target => {
                return format!("{link_display} symlink already correct (-> {relative_target})");
            }
            Ok(existing) => {
                if let Err(e) = fs::remove_file(link_path) {
                    return format!("Failed to remove stale symlink {link_display}: {e}");
                }
                log::debug!(
                    "loom-daemon init (dogfood): updating {link_display} symlink: {} -> \
                     {relative_target}",
                    existing.display()
                );
            }
            Err(_) => {
                // Unreadable (race, permissions) — treat like any other
                // stale symlink and try to replace it.
                if let Err(e) = fs::remove_file(link_path) {
                    return format!("Failed to remove stale symlink {link_display}: {e}");
                }
            }
        },
        Ok(meta) if meta.is_dir() => {
            let extra = local_only_files(link_path, source_abs);
            if !extra.is_empty() {
                return format!(
                    "Skipped {link_display} symlink: local-only file(s) present ({}) — refusing \
                     to replace with a symlink; move or commit them, then re-run",
                    extra.join(", ")
                );
            }
            if let Err(e) = fs::remove_dir_all(link_path) {
                return format!("Failed to remove {link_display} before symlinking: {e}");
            }
        }
        Ok(_) => {
            // A plain file occupies the path — never touch it.
            return format!("Skipped {link_display} symlink: a regular file occupies the path");
        }
        Err(_) => {
            // Doesn't exist — fall through to create.
        }
    }

    #[cfg(unix)]
    {
        if let Err(e) = std::os::unix::fs::symlink(relative_target, link_path) {
            return format!("Failed to create {link_display} symlink -> {relative_target}: {e}");
        }
    }
    #[cfg(not(unix))]
    {
        return format!(
            "Skipped {link_display} symlink: dogfood symlink creation is Unix-only (#6440)"
        );
    }

    format!("Created {link_display} symlink -> {relative_target}")
}

/// Idempotently create Loom's own dogfood symlinks — `.claude/commands/loom`
/// and `.claude/agents`, both pointing into this same repo's `defaults/` tree
/// — for a `workspace_path` already confirmed to be the Loom source repo
/// (Issue #6440).
///
/// **Scoping is load-bearing.** This function performs filesystem writes
/// (symlink creation, and — in the local-only-files-absent case — removal of
/// a real `.claude/commands/loom/` or `.claude/agents/` directory) that would
/// silently corrupt a CONSUMER repo's tracked, real `.claude/commands/loom/`
/// files if ever invoked outside the dogfood case. It is therefore `pub(super)`
/// (visible only to [`super::initialize_workspace`]'s own `is_loom_source_repo`
/// branch), never re-exported past the `init` module, and every caller MUST
/// gate it behind [`is_loom_source_repo`] first — mirroring the shell
/// installer's own `--dogfood` / `is_loom_source_repo` gating in
/// `scripts/install-loom.sh` and `scripts/install/dogfood-commands.sh`.
///
/// Returns one human-readable outcome line per symlink, for both `log::info!`
/// at the call site and direct test assertions — never an error the caller
/// must propagate; every failure mode inside [`ensure_dir_symlink`] is
/// advisory-only.
pub(super) fn link_dogfood_symlinks(workspace_path: &Path) -> Vec<String> {
    let mut lines = Vec::new();

    // `.claude/commands/` itself must stay a REAL directory (issue #3682): a
    // co-installed tool's sibling namespace (`.claude/commands/repo/...`)
    // must not write through a directory-level symlink into `defaults/`. A
    // legacy whole-dir symlink (pre-#3682) is removed first so the
    // `ensure_dir_symlink` call below can build a real `.claude/commands/`
    // directory containing just the scoped `loom/` symlink.
    let commands_dir = workspace_path.join(".claude").join("commands");
    if fs::symlink_metadata(&commands_dir)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        if let Err(e) = fs::remove_file(&commands_dir) {
            lines.push(format!("Failed to remove legacy .claude/commands symlink: {e}"));
        } else {
            lines.push("Removed legacy .claude/commands symlink (pre-#3682)".to_string());
        }
    }

    let commands_source = workspace_path
        .join("defaults")
        .join(".claude")
        .join("commands")
        .join("loom");
    let commands_link = commands_dir.join("loom");
    lines.push(ensure_dir_symlink(
        &commands_link,
        "../../defaults/.claude/commands/loom",
        &commands_source,
    ));

    let agents_source = workspace_path
        .join("defaults")
        .join(".claude")
        .join("agents");
    let agents_link = workspace_path.join(".claude").join("agents");
    lines.push(ensure_dir_symlink(&agents_link, "../defaults/.claude/agents", &agents_source));

    lines
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

    // ------------------------------------------------------------------
    // Dogfood symlink creation (#6440)
    // ------------------------------------------------------------------

    #[cfg(unix)]
    fn is_symlink(path: &Path) -> bool {
        fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn build_loom_source_repo(temp: &TempDir) -> PathBuf {
        let workspace = temp.path();
        fs::create_dir_all(workspace.join("loom-api")).unwrap();
        fs::create_dir_all(workspace.join("loom-daemon")).unwrap();
        fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
        fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();
        let cmds = workspace
            .join("defaults")
            .join(".claude")
            .join("commands")
            .join("loom");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(cmds.join("sweep.md"), "# sweep").unwrap();
        let agents = workspace.join("defaults").join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("loom-builder.md"), "# builder").unwrap();
        workspace.to_path_buf()
    }

    #[cfg(unix)]
    #[test]
    fn test_link_dogfood_symlinks_creates_both_links() {
        let temp = TempDir::new().unwrap();
        let workspace = build_loom_source_repo(&temp);

        let lines = link_dogfood_symlinks(&workspace);
        assert!(lines
            .iter()
            .any(|l| l.contains("Created") && l.contains("commands/loom")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Created") && l.contains("agents")));

        let commands_link = workspace.join(".claude").join("commands").join("loom");
        let agents_link = workspace.join(".claude").join("agents");
        assert!(is_symlink(&commands_link), "commands/loom must be a symlink");
        assert!(is_symlink(&agents_link), "agents must be a symlink");
        assert_eq!(
            fs::read_link(&commands_link).unwrap(),
            PathBuf::from("../../defaults/.claude/commands/loom")
        );
        assert_eq!(
            fs::read_link(&agents_link).unwrap(),
            PathBuf::from("../defaults/.claude/agents")
        );

        // The content resolves through the symlink.
        assert!(commands_link.join("sweep.md").is_file());
        assert!(agents_link.join("loom-builder.md").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn test_link_dogfood_symlinks_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let workspace = build_loom_source_repo(&temp);

        link_dogfood_symlinks(&workspace);
        let second = link_dogfood_symlinks(&workspace);

        assert!(
            second
                .iter()
                .any(|l| l.contains("already correct") && l.contains("commands/loom")),
            "second run must be a no-op for commands/loom, got: {second:?}"
        );
        assert!(
            second
                .iter()
                .any(|l| l.contains("already correct") && l.contains("agents")),
            "second run must be a no-op for agents, got: {second:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_link_dogfood_symlinks_preserves_local_only_files() {
        let temp = TempDir::new().unwrap();
        let workspace = build_loom_source_repo(&temp);

        // A stale, real .claude/agents/ directory holding a file NOT present
        // under defaults/.claude/agents/ — must never be silently discarded
        // (issue #6440's complexity note: getting this scoping/safety wrong
        // could corrupt real content).
        let agents_dir = workspace.join(".claude").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("local-only.md"), "not in defaults/").unwrap();

        let lines = link_dogfood_symlinks(&workspace);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Skipped") && l.contains("local-only")),
            "expected a local-only-files skip line, got: {lines:?}"
        );
        assert!(!is_symlink(&agents_dir), "must not replace a dir with local-only content");
        assert!(agents_dir.join("local-only.md").is_file(), "local-only file must survive");
    }

    #[cfg(unix)]
    #[test]
    fn test_link_dogfood_symlinks_replaces_stale_copy_without_local_only_files() {
        let temp = TempDir::new().unwrap();
        let workspace = build_loom_source_repo(&temp);

        // A stale, real .claude/agents/ directory whose only content is
        // ALSO present under defaults/.claude/agents/ (the pre-symlink
        // materialized-copy shape) — safe to replace with the symlink.
        let agents_dir = workspace.join(".claude").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("loom-builder.md"), "stale copy").unwrap();

        link_dogfood_symlinks(&workspace);
        assert!(
            is_symlink(&agents_dir),
            "a stale copy with no local-only files must be replaced"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_link_dogfood_symlinks_soft_skips_missing_source() {
        let temp = TempDir::new().unwrap();
        // A workspace with no defaults/.claude/{commands,agents} at all.
        let workspace = temp.path();
        fs::create_dir_all(workspace).unwrap();

        let lines = link_dogfood_symlinks(workspace);
        assert!(lines.iter().all(|l| l.starts_with("Skipped")), "got: {lines:?}");
        assert!(!workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .exists());
        assert!(!workspace.join(".claude").join("agents").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_link_dogfood_symlinks_removes_legacy_whole_dir_symlink() {
        let temp = TempDir::new().unwrap();
        let workspace = build_loom_source_repo(&temp);

        // Pre-#3682 shape: the WHOLE `.claude/commands` dir was a symlink
        // into defaults/. Must be replaced by a real `.claude/commands/` dir
        // containing only a scoped `loom/` symlink, so a sibling namespace
        // (e.g. `.claude/commands/repo/...`) never writes through into
        // `defaults/`.
        fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::os::unix::fs::symlink(
            "../defaults/.claude/commands",
            workspace.join(".claude").join("commands"),
        )
        .unwrap();

        let lines = link_dogfood_symlinks(&workspace);
        assert!(lines.iter().any(|l| l.contains("legacy")), "got: {lines:?}");

        let commands_dir = workspace.join(".claude").join("commands");
        assert!(
            !is_symlink(&commands_dir),
            "the whole commands/ dir must be real, not a symlink"
        );
        assert!(is_symlink(&commands_dir.join("loom")), "only the loom/ subdir is symlinked");
    }
}
