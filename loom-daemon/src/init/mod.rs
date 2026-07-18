//! Loom workspace initialization module
//!
//! This module provides functionality for initializing Loom workspaces
//! from the daemon and CLI surface. It can be used from:
//! - CLI mode (loom-daemon init)
//! - MCP tools (shared code)
//!
//! The initialization process:
//! 1. Validates the target is a git repository
//! 2. Detects self-installation (Loom source repo) and runs validation-only mode
//! 3. Copies `.loom/` configuration from `defaults/` (merge mode preserves custom files)
//! 4. Sets up repository scaffolding (CLAUDE.md, .claude/, .codex/)
//! 5. Updates .gitignore with Loom ephemeral patterns
//! 6. Reports which files were preserved vs added
//!
//! # Module Structure
//!
//! - [`git`]: Git detection, validation, and path resolution
//! - [`file_ops`]: File copy/merge/clean operations with reporting
//! - [`templates`]: Template variable substitution
//! - [`scaffolding`]: Repository scaffolding setup (CLAUDE.md, .claude/, etc.)
//! - [`post_init`]: Post-initialization operations (manifest, gitignore)

mod file_ops;
mod git;
mod post_init;
mod retired;
mod scaffolding;
mod templates;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use file_ops::{clean_managed_dir, copy_dir_with_report, verify_copied_files, TemplateContext};
use post_init::{find_overbroad_loom_patterns, generate_manifest, update_gitignore};
use retired::cleanup_retired_files;
use scaffolding::setup_repository_scaffolding;

// Re-export public types and functions
pub use git::is_loom_source_repo;

// Import the rest for internal use
use git::{resolve_defaults_path, validate_git_repository, validate_loom_source_repo};

/// Report of files affected during initialization
///
/// This struct tracks which files were added from defaults vs preserved
/// from the existing installation, enabling users to identify custom files
/// and deprecated files that may need cleanup.
#[derive(Debug, Default)]
pub struct InitReport {
    /// Files that were added from defaults (didn't exist before)
    pub added: Vec<String>,
    /// Files that were preserved (existed before, not overwritten)
    pub preserved: Vec<String>,
    /// Files that were updated (existed before, overwritten on reinstall)
    pub updated: Vec<String>,
    /// Files that were removed (existed in destination but not in source, cleaned on reinstall)
    pub removed: Vec<String>,
    /// Files that failed post-copy verification (destination doesn't match source)
    pub verification_failures: Vec<String>,
    /// Whether this was a self-installation (Loom source repo)
    pub is_self_install: bool,
    /// Validation results for self-installation mode
    pub validation: Option<ValidationReport>,
}

/// Validation report for self-installation mode
#[derive(Debug, Default)]
pub struct ValidationReport {
    /// Role definitions found
    pub roles_found: Vec<String>,
    /// Scripts found
    pub scripts_found: Vec<String>,
    /// Slash commands found
    pub commands_found: Vec<String>,
    /// Subagent definitions found in .claude/agents/
    pub agents_found: Vec<String>,
    /// Whether CLAUDE.md exists
    pub has_claude_md: bool,
    /// Whether .github/labels.yml exists
    pub has_labels_yml: bool,
    /// Issues found during validation
    pub issues: Vec<String>,
}

/// Initialize a Loom workspace in the target directory
///
/// # Arguments
///
/// * `workspace_path` - Path to the workspace directory (must be a git repository)
/// * `defaults_path` - Path to the defaults directory (usually "defaults" or bundled resource)
/// * `force` - If true, overwrite existing files (otherwise merge mode preserves custom files)
///
/// # Returns
///
/// * `Ok(InitReport)` - Workspace successfully initialized with report of changes
/// * `Err(String)` - Initialization failed with error message
///
/// # Behavior
///
/// - **Fresh install** (no .loom directory): Copies all files from defaults
/// - **Reinstall with force=false** (merge mode): Adds new files, preserves ALL existing files
/// - **Reinstall with force=true** (force-merge mode): Updates default files, preserves custom files
///
/// Both reinstall modes preserve custom project roles/commands (files not in defaults).
/// Force mode is useful when you want to update Loom's built-in roles to the latest version.
///
/// # Errors
///
/// This function will return an error if:
/// - The workspace path doesn't exist or isn't a directory
/// - The workspace isn't a git repository (no .git directory)
/// - File operations fail (insufficient permissions, disk full, etc.)
pub fn initialize_workspace(
    workspace_path: &str,
    defaults_path: &str,
    force: bool,
) -> Result<InitReport, String> {
    let workspace = Path::new(workspace_path);
    let loom_path = workspace.join(".loom");
    let mut report = InitReport::default();

    // Validate workspace is a git repository
    validate_git_repository(workspace_path)?;

    // Check for over-broad gitignore patterns that would shadow installed Loom
    // files (e.g., `.loom/scripts/lib/*.sh`). This catches the regression
    // reported in issue #3287, where a target repo's `.gitignore` contained
    // `.loom/` and caused the install worktree's lib files to never be
    // committed to main. We fail fast here, before any file operations.
    let bad_patterns = find_overbroad_loom_patterns(workspace);
    if !bad_patterns.is_empty() {
        return Err(format!(
            "Refusing to install: .gitignore contains pattern(s) that would block \
             installed Loom files from being committed (e.g., .loom/scripts/lib/*.sh). \
             Remove or scope these patterns to specific runtime files before \
             reinstalling. Offending patterns: {}",
            bad_patterns.join(", ")
        ));
    }

    // Check for self-installation (Loom source repo)
    if is_loom_source_repo(workspace) {
        report.is_self_install = true;
        report.validation = Some(validate_loom_source_repo(workspace));
        update_gitignore(workspace)?;
        return Ok(report);
    }

    // Resolve defaults path (development mode or bundled resource)
    let defaults = resolve_defaults_path(defaults_path)?;
    let is_reinstall = loom_path.exists();
    let _ = (is_reinstall, force); // These affect behavior in called functions

    // Create .loom directory if it doesn't exist
    fs::create_dir_all(&loom_path).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy config and README files.
    //
    // `config.json` is merge-aware (issue #3598): unlike the README (a
    // Loom-owned doc that is safe to overwrite), `.loom/config.json` is
    // committed CONSUMER configuration that may carry local overrides such as
    // `worktree.root`. A bare `fs::copy` from the template would silently drop
    // those keys — see `merge_config_file`.
    merge_config_file(&defaults, &loom_path, &mut report)?;
    copy_single_file(&defaults, &loom_path, ".loom-README.md", ".loom/README.md", &mut report)?;

    // Sync managed directories (clean stale files on reinstall, then copy fresh)
    sync_managed_dir(&defaults, &loom_path, "roles", is_reinstall, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "scripts", is_reinstall, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "hooks", is_reinstall, &mut report)?;
    // `docs` ships static reference documentation (e.g. ci-integration.md
    // from issue #3333). Sync alongside other managed dirs so installed
    // repos always carry the latest copy.
    sync_managed_dir(&defaults, &loom_path, "docs", is_reinstall, &mut report)?;

    // Sync `.loom/bin/` from `defaults/.loom/bin/`. The manifest generator
    // (scripts/install/manifest.sh) walks `defaults/.loom/` and registers
    // every file under it as Loom-installed, so the bin/ subdirectory must
    // be copied here or the post-install metadata-vs-disk verification
    // fails fast on missing `.loom/bin/loom`. Pass `defaults/.loom` as the
    // helper's `defaults` arg so src=`defaults/.loom/bin` and dst=`.loom/bin`.
    sync_managed_dir(&defaults.join(".loom"), &loom_path, "bin", is_reinstall, &mut report)?;

    make_shell_scripts_executable(&loom_path.join("hooks"));
    make_shell_scripts_executable(&loom_path.join("scripts"));
    make_shell_scripts_executable(&loom_path.join("bin"));

    // Update .gitignore and setup scaffolding
    update_gitignore(workspace)?;
    setup_repository_scaffolding(workspace, &defaults, force, &mut report)?;

    // Verify all copied files match their sources
    verify_all_copied_files(workspace, &defaults, &loom_path, &mut report);

    // Filter out verification failures for files that were intentionally preserved.
    // Preserved files (existing user customizations) are expected to differ from the
    // source defaults — flagging them as failures is misleading and the prior
    // "rerun with --force" remediation would clobber the user's intentional edits.
    filter_preserved_from_verification_failures(&mut report);

    // Content-gated cleanup of retired Loom strays (issue #3576). The daemon
    // init sync is source-driven and never removes destination-only files, so a
    // stray `.claude/commands/loom/release.md` (the `/loom:release` skill
    // retired by #3563) lingers on disk for Quick-Install consumers. Remove it
    // iff its sha256 matches a frozen shipped digest (unmodified); preserve a
    // customized copy; no-op when absent. Mirrors the shell-side
    // `LOOM_RETIRED_FILES` block in scripts/install-loom.sh (PR #3575).
    //
    // Placed after scaffolding (post-sync) and before generate_manifest so the
    // manifest reflects on-disk state. The self-install short-circuit above
    // (returns at ~line 147 before scaffolding) means this never runs on the
    // Loom source repo — it must not mutate the source tree.
    cleanup_retired_files(workspace, &mut report);

    // Generate installation manifest (.loom/manifest.json)
    generate_manifest(workspace);

    Ok(report)
}

/// Remove `verification_failures` entries whose path appears in `preserved`.
///
/// Verification failure entries are formatted as `"{rel_path} ({reason})"`. We
/// extract the leading path component and drop the entry if it matches a
/// preserved path. A preserved file is, by definition, expected to differ from
/// the source default (the user customized it), so it should not surface as a
/// "verification failure".
fn filter_preserved_from_verification_failures(report: &mut InitReport) {
    if report.preserved.is_empty() || report.verification_failures.is_empty() {
        return;
    }
    let preserved: HashSet<&str> = report.preserved.iter().map(String::as_str).collect();
    report.verification_failures.retain(|f| {
        let rel_path = f.split(" (").next().unwrap_or(f.as_str());
        !preserved.contains(rel_path)
    });
}

/// Copy a single file from defaults to the loom directory, tracking in report.
fn copy_single_file(
    defaults: &Path,
    loom_path: &Path,
    src_name: &str,
    report_name: &str,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(src_name);
    // The destination may differ from the source name (e.g., ".loom-README.md" → "README.md")
    let dst_name = report_name.strip_prefix(".loom/").unwrap_or(src_name);
    let dst = loom_path.join(dst_name);
    if src.exists() {
        let existed = dst.exists();
        fs::copy(&src, &dst).map_err(|e| format!("Failed to copy {src_name}: {e}"))?;
        if existed {
            report.updated.push(report_name.to_string());
        } else {
            report.added.push(report_name.to_string());
        }
    }
    Ok(())
}

/// Merge-aware copy of `.loom/config.json` from defaults (issue #3598).
///
/// `.loom/config.json` is committed consumer configuration, not a runtime
/// artifact. A bare `fs::copy` (as `copy_single_file` performs) would clobber
/// consumer keys such as the documented `worktree.root` override every time the
/// installer reran. This function instead:
///
/// - **Destination missing** → the template is parsed and re-emitted through
///   the same canonical `to_string_pretty` serialize path the merge branch
///   uses (issue #3619), recorded as `added`. Routing the fresh install through
///   serialization (rather than a raw `fs::copy` of the hand-formatted
///   template) makes the on-disk file canonical from the very first install, so
///   a later reinstall merge re-emits byte-identical output and config.json is
///   never left dirty. (If the template is not valid JSON, falls back to a raw
///   copy.)
/// - **Destination is a valid JSON object** → deep-merge with the shipped
///   template as the base and the **existing consumer values winning** on
///   conflict; keys new in the template are added, unknown consumer keys at any
///   depth are preserved. Written with deterministic pretty serialization so
///   repeat reinstalls are byte-idempotent. Recorded as `preserved`.
/// - **Destination exists but is invalid JSON (or not an object)** → fall back
///   to a template copy with a loud warning; the install does not abort.
///   Recorded as `updated`.
///
/// Byte-exact preservation of consumer formatting/comments is explicitly out of
/// scope — deterministic re-serialization is acceptable as long as keys/values
/// survive and repeat runs are stable.
fn merge_config_file(
    defaults: &Path,
    loom_path: &Path,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join("config.json");
    let dst = loom_path.join("config.json");
    let report_name = ".loom/config.json";

    // No template shipped — nothing to do (mirrors copy_single_file's guard).
    if !src.exists() {
        return Ok(());
    }

    let template_str = fs::read_to_string(&src)
        .map_err(|e| format!("Failed to read defaults config.json: {e}"))?;

    // Fresh install: no existing consumer file → write the template through the
    // SAME canonical serialize path the reinstall merge uses (issue #3619). A
    // bare `fs::copy` of the hand-formatted template produced bytes that a later
    // merge's `to_string_pretty` output could never match, leaving config.json
    // permanently dirty after the first reinstall. Serializing here makes the
    // on-disk file canonical from the very first install, so any later merge
    // re-emits byte-identical output. With serde_json's `preserve_order`
    // feature, template key order is retained (keys are not alphabetized).
    if !dst.exists() {
        match serde_json::from_str::<Value>(&template_str) {
            Ok(template_val) => {
                let mut serialized = serde_json::to_string_pretty(&template_val)
                    .map_err(|e| format!("Failed to serialize config.json: {e}"))?;
                serialized.push('\n');
                fs::write(&dst, serialized)
                    .map_err(|e| format!("Failed to write config.json: {e}"))?;
            }
            Err(e) => {
                // Template is invalid JSON — fall back to a raw copy rather than
                // dropping the install. (The reinstall branch handles this too.)
                eprintln!(
                    "Warning: defaults/config.json is not valid JSON ({e}); \
                     copying it verbatim to .loom/config.json"
                );
                fs::copy(&src, &dst).map_err(|e| format!("Failed to copy config.json: {e}"))?;
            }
        }
        report.added.push(report_name.to_string());
        return Ok(());
    }

    let existing_str = fs::read_to_string(&dst)
        .map_err(|e| format!("Failed to read existing config.json: {e}"))?;

    // If the shipped template is somehow invalid JSON, do NOT clobber the
    // consumer's file — leave it exactly as-is and record it as preserved.
    let template_val: Value = match serde_json::from_str(&template_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "Warning: defaults/config.json is not valid JSON ({e}); \
                 leaving existing .loom/config.json untouched"
            );
            report.preserved.push(report_name.to_string());
            return Ok(());
        }
    };

    // If the consumer file is missing/invalid/non-object, fall back to the
    // template copy with a loud warning. This must not abort the install.
    let existing_val: Value = match serde_json::from_str::<Value>(&existing_str) {
        Ok(v) if v.is_object() => v,
        _ => {
            eprintln!(
                "Warning: existing .loom/config.json is not valid JSON; overwriting \
                 with the shipped template (previous contents were not preserved)"
            );
            fs::copy(&src, &dst).map_err(|e| format!("Failed to copy config.json: {e}"))?;
            report.updated.push(report_name.to_string());
            return Ok(());
        }
    };

    // Deep-merge: template is the base, existing consumer values win on conflict.
    let mut merged = template_val;
    deep_merge_existing_wins(&mut merged, &existing_val);

    let mut serialized = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("Failed to serialize merged config.json: {e}"))?;
    serialized.push('\n');
    fs::write(&dst, serialized).map_err(|e| format!("Failed to write merged config.json: {e}"))?;
    report.preserved.push(report_name.to_string());
    Ok(())
}

/// Deep-merge `overlay` into `base` with **overlay values winning** on conflict.
///
/// Used by [`merge_config_file`] with `base` = shipped template and `overlay` =
/// existing consumer config, so consumer edits are never lost while new
/// template keys are still delivered:
///
/// - Two objects are merged key-by-key, recursing into nested objects.
/// - Any non-object `overlay` value (array, scalar, null) replaces the
///   corresponding `base` value wholesale.
/// - Keys present only in `base` (new template keys) are retained.
/// - Keys present only in `overlay` (unknown consumer keys) are preserved.
fn deep_merge_existing_wins(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                match base_map.get_mut(key) {
                    Some(base_val) => deep_merge_existing_wins(base_val, overlay_val),
                    None => {
                        base_map.insert(key.clone(), overlay_val.clone());
                    }
                }
            }
        }
        (base_slot, overlay_val) => {
            *base_slot = overlay_val.clone();
        }
    }
}

/// Sync a managed directory: clean stale files on reinstall, then copy fresh from defaults.
///
/// After copying, this function performs a fail-fast assertion that every file
/// (including those in subdirectories) present in `defaults/<dir_name>/` exists
/// in the destination. This guards against silent omissions like the regression
/// reported in issue #3220, where `scripts/lib/forge-helpers.sh` was missing
/// from installs even though `lib/loom-tools.sh` was verified by name.
fn sync_managed_dir(
    defaults: &Path,
    loom_path: &Path,
    dir_name: &str,
    is_reinstall: bool,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(dir_name);
    let dst = loom_path.join(dir_name);
    let report_prefix = format!(".loom/{dir_name}");
    if src.exists() {
        if is_reinstall {
            clean_managed_dir(&dst, &report_prefix, report)
                .map_err(|e| format!("Failed to clean {dir_name} directory: {e}"))?;
        }
        copy_dir_with_report(&src, &dst, &report_prefix, report)
            .map_err(|e| format!("Failed to copy {dir_name} directory: {e}"))?;

        // Fail-fast: ensure every source file (including subdirectories) reached
        // the destination. This catches bugs in copy_dir_with_report and
        // unexpected filesystem failures (permission denied on a subdir, etc.)
        // before they propagate to a broken install.
        let missing = find_missing_files(&src, &dst);
        if !missing.is_empty() {
            return Err(format!(
                "Sync of {dir_name} directory completed but {} file(s) are missing from \
                 destination (likely a copy bug or filesystem error): {}",
                missing.len(),
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// Recursively find files present in `src` but missing from `dst`.
///
/// Returns relative paths (from `src`) for each missing file. Used as a
/// post-copy assertion to detect partial copies. Subdirectories are walked
/// recursively so that nested files (e.g., `scripts/lib/*.sh`) are checked.
fn find_missing_files(src: &Path, dst: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    collect_missing_files(src, dst, "", &mut missing);
    missing
}

fn collect_missing_files(src: &Path, dst: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();
        let rel_path = if prefix.is_empty() {
            file_name_str.to_string()
        } else {
            format!("{prefix}/{file_name_str}")
        };
        let src_child = entry.path();
        let dst_child = dst.join(&file_name);
        if file_type.is_dir() {
            collect_missing_files(&src_child, &dst_child, &rel_path, out);
        } else if !dst_child.exists() {
            out.push(rel_path);
        }
    }
}

/// Ensure all `.sh` files in a directory (and subdirectories) are executable.
///
/// This is applied to both hooks/ and scripts/ after copying from defaults.
/// While `fs::copy` preserves permissions on Unix, some git configurations
/// or filesystem operations may strip the execute bit. This ensures all
/// shell scripts remain executable regardless of how they were copied.
fn make_shell_scripts_executable(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                make_shell_scripts_executable(&path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("sh") {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = std::fs::metadata(&path) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(perms.mode() | 0o111);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
            }
        }
    }
}

/// Verify all copied files and scaffolding directories match their sources.
fn verify_all_copied_files(
    workspace: &Path,
    defaults: &Path,
    loom_path: &Path,
    report: &mut InitReport,
) {
    // Verify .loom managed directories (no template substitution needed)
    for dir_name in &["roles", "scripts", "hooks", "docs"] {
        let src = defaults.join(dir_name);
        let dst = loom_path.join(dir_name);
        let prefix = format!(".loom/{dir_name}");
        verify_copied_files(&src, &dst, &prefix, report, None);
    }

    // Verify scaffolding directories with template context for variable substitution
    let repo_info = git::extract_repo_info(workspace);
    let template_ctx = TemplateContext {
        repo_owner: repo_info.as_ref().map(|(o, _)| o.clone()),
        repo_name: repo_info.map(|(_, n)| n),
        loom_metadata: templates::LoomMetadata::from_env(),
    };
    let ctx = Some(&template_ctx);

    for dir_name in &[".claude", ".codex", ".github"] {
        let src = defaults.join(dir_name);
        let dst = workspace.join(dir_name);
        verify_copied_files(&src, &dst, dir_name, report, ctx);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_is_loom_source_repo_marker_file() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Initially not a Loom source repo
        assert!(!is_loom_source_repo(workspace));

        // Create marker file
        fs::write(workspace.join(".loom-source"), "").unwrap();

        // Now it should be detected as Loom source repo
        assert!(is_loom_source_repo(workspace));
    }

    #[test]
    fn test_is_loom_source_repo_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Initially not a Loom source repo
        assert!(!is_loom_source_repo(workspace));

        // Create partial structure (not enough)
        fs::create_dir(workspace.join("loom-api")).unwrap();
        assert!(!is_loom_source_repo(workspace));

        // Create more structure
        fs::create_dir(workspace.join("loom-daemon")).unwrap();
        assert!(!is_loom_source_repo(workspace));

        // Create defaults directory
        fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
        fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

        // Now it should be detected as Loom source repo
        assert!(is_loom_source_repo(workspace));
    }

    #[test]
    fn test_self_install_returns_validation_report() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Create git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create Loom source structure
        fs::create_dir(workspace.join("loom-api")).unwrap();
        fs::create_dir(workspace.join("loom-daemon")).unwrap();
        fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
        fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

        // Create minimal .loom structure
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
        fs::write(workspace.join(".loom").join("roles").join("builder.md"), "").unwrap();
        fs::write(workspace.join(".loom").join("scripts").join("worktree.sh"), "").unwrap();

        // Create .claude/commands/loom/
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        for cmd in [
            "builder.md",
            "judge.md",
            "curator.md",
            "doctor.md",
            "shepherd.md",
        ] {
            fs::write(
                workspace
                    .join(".claude")
                    .join("commands")
                    .join("loom")
                    .join(cmd),
                "",
            )
            .unwrap();
        }

        // Create .claude/agents/ (subagent definitions — required for
        // native Claude Code subagent dispatch). See issue #3310.
        fs::create_dir_all(workspace.join(".claude").join("agents")).unwrap();
        for agent in [
            "loom-builder.md",
            "loom-judge.md",
            "loom-curator.md",
            "loom-doctor.md",
            "loom-shepherd.md",
        ] {
            fs::write(workspace.join(".claude").join("agents").join(agent), "").unwrap();
        }

        // Create roles to satisfy the >=5 role-count check (issue #3310 makes
        // agents/ subject to the same kind of minimum-count audit and the
        // validation report now bails on too-few defaults across the board).
        for role in [
            "builder.md",
            "judge.md",
            "curator.md",
            "doctor.md",
            "shepherd.md",
        ] {
            fs::write(workspace.join(".loom").join("roles").join(role), "").unwrap();
        }

        // Create a couple of scripts to satisfy the >=2 script-count check.
        fs::write(workspace.join(".loom").join("scripts").join("daemon.sh"), "").unwrap();

        // Create docs
        fs::write(workspace.join("CLAUDE.md"), "").unwrap();

        // Create labels.yml
        fs::create_dir_all(workspace.join(".github")).unwrap();
        fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

        // Run initialization
        let result = initialize_workspace(
            workspace.to_str().unwrap(),
            "nonexistent-defaults", // Should not be used for self-install
            false,
        );

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify self-install detection
        assert!(report.is_self_install);
        assert!(report.validation.is_some());

        let validation = report.validation.unwrap();
        assert!(validation.roles_found.contains(&"builder".to_string()));
        assert!(validation.scripts_found.contains(&"worktree".to_string()));
        assert!(validation.commands_found.contains(&"builder".to_string()));
        // Issue #3310: subagent definitions must be discovered for native
        // Claude Code `subagent_type` dispatch to work.
        assert!(validation
            .agents_found
            .contains(&"loom-builder".to_string()));
        assert!(validation.has_claude_md);
        assert!(validation.has_labels_yml);
        // Verify the missing-agents-directory issue is NOT raised when
        // the directory exists with the expected fixtures.
        assert!(!validation
            .issues
            .iter()
            .any(|i| i.contains("Missing .claude/agents/")));
    }

    #[test]
    fn test_self_install_flags_missing_agents_directory() {
        // Issue #3310: a self-installed Loom checkout that is missing
        // `.claude/agents/` cannot dispatch subagents. The validation
        // report must surface this as an explicit issue so downstream
        // tooling (installer reconciliation, `loom-daemon init`) can
        // fail loudly instead of silently producing a broken install.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        // Mark this directory as a Loom source repo via the marker file.
        // We deliberately do NOT create `.claude/agents/` here.
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::write(workspace.join(".loom-source"), "").unwrap();
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        fs::write(workspace.join("CLAUDE.md"), "").unwrap();
        fs::create_dir_all(workspace.join(".github")).unwrap();
        fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
        assert!(result.is_ok());

        let report = result.unwrap();
        assert!(report.is_self_install);
        let validation = report.validation.expect("validation report present");
        assert!(validation.agents_found.is_empty());
        assert!(
            validation
                .issues
                .iter()
                .any(|i| i == "Missing .claude/agents/ directory"),
            "Expected missing-agents-directory issue, got: {:?}",
            validation.issues
        );
    }

    #[test]
    fn test_self_install_skips_retired_file_cleanup() {
        // Issue #3576: the retired-file cleanup is placed AFTER the self-install
        // short-circuit in `initialize_workspace`, so it must never touch the
        // Loom source tree. A stray `.claude/commands/loom/release.md` in a
        // self-install workspace is left exactly as-is: not removed, and not
        // even recorded in `report.preserved` (which would signal the cleanup
        // ran and evaluated it — i.e. the call was misplaced before the early
        // return).
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::write(workspace.join(".loom-source"), "").unwrap();
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        fs::create_dir_all(workspace.join(".claude").join("agents")).unwrap();
        fs::write(workspace.join("CLAUDE.md"), "").unwrap();
        fs::create_dir_all(workspace.join(".github")).unwrap();
        fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

        // A retired stray on disk in the source tree.
        let stray = workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("release.md");
        fs::write(&stray, "some release.md content\n").unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
        assert!(result.is_ok());
        let report = result.unwrap();

        assert!(report.is_self_install);
        // Cleanup never ran: file untouched, and it appears in neither list.
        assert!(stray.exists(), "self-install must not remove the stray release.md");
        let retired = ".claude/commands/loom/release.md".to_string();
        assert!(!report.removed.contains(&retired));
        assert!(!report.preserved.contains(&retired));
    }

    #[test]
    fn test_roles_cleaned_and_updated_on_reinstall() {
        // On reinstall, managed directories (roles/, scripts/) are cleaned first
        // to remove stale files, then fresh defaults are copied in.
        // Custom files that aren't in defaults are removed (not preserved).
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with roles
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "new builder content v2").unwrap();
        fs::write(defaults.join("roles").join("judge.md"), "new judge content").unwrap();

        // Create existing .loom directory (simulates previous install)
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::write(
            workspace.join(".loom").join("roles").join("builder.md"),
            "old builder content v1",
        )
        .unwrap();
        fs::write(workspace.join(".loom").join("roles").join("stale-role.md"), "stale role")
            .unwrap();

        // Run initialization WITHOUT force flag (simulates normal reinstall)
        let result = initialize_workspace(
            workspace.to_str().unwrap(),
            defaults.to_str().unwrap(),
            false, // No force flag
        );

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify: builder.md has new content from defaults
        let builder =
            fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
        assert_eq!(builder, "new builder content v2");

        // Verify: judge.md was ADDED (new default role)
        let judge =
            fs::read_to_string(workspace.join(".loom").join("roles").join("judge.md")).unwrap();
        assert_eq!(judge, "new judge content");

        // Verify: stale-role.md was REMOVED (not in defaults)
        assert!(
            !workspace
                .join(".loom")
                .join("roles")
                .join("stale-role.md")
                .exists(),
            "Stale role file should have been removed on reinstall"
        );

        // Verify report reflects the removal
        assert!(
            report
                .removed
                .contains(&".loom/roles/stale-role.md".to_string()),
            "Report should list stale-role.md as removed, got: {:?}",
            report.removed
        );

        // Both files from defaults should be reported as added (directory was cleaned first)
        assert!(report.added.contains(&".loom/roles/builder.md".to_string()));
        assert!(report.added.contains(&".loom/roles/judge.md".to_string()));
    }

    #[test]
    fn test_loom_bin_directory_copied_on_install() {
        // Regression: `.loom/bin/` (the loom CLI wrapper) must be copied from
        // `defaults/.loom/bin/`. The install manifest generator walks
        // `defaults/.loom/` and lists `.loom/bin/loom` as a shipped file, so
        // if initialize_workspace omits the copy, the installer's
        // post-install metadata-vs-disk check fails with
        // "MISSING: .loom/bin/loom" and rolls the whole install back.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = workspace.join("defaults");

        // Minimal git repo + defaults.
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(&defaults).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();

        // Ship a loom CLI wrapper under defaults/.loom/bin/.
        fs::create_dir_all(defaults.join(".loom").join("bin")).unwrap();
        let wrapper = "#!/usr/bin/env bash\necho loom\n";
        fs::write(defaults.join(".loom").join("bin").join("loom"), wrapper).unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok(), "init failed: {result:?}");

        // The wrapper must land at .loom/bin/loom with identical contents.
        let installed = workspace.join(".loom").join("bin").join("loom");
        assert!(installed.exists(), ".loom/bin/loom should be copied from defaults/.loom/bin/");
        assert_eq!(fs::read_to_string(&installed).unwrap(), wrapper);
    }

    #[test]
    fn test_reinstall_removes_stale_files() {
        // Verifies that files in destination but not in source are removed on reinstall
        // This is the core behavior change for issue #1798
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with scripts (simulates Python port: old shell scripts removed)
        fs::create_dir_all(defaults.join("scripts")).unwrap();
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("scripts").join("worktree.sh"), "#!/bin/bash\n# kept").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder role").unwrap();

        // Create existing .loom with stale scripts (simulates pre-port state)
        fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
        fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
        fs::write(
            workspace.join(".loom").join("scripts").join("worktree.sh"),
            "#!/bin/bash\n# old",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("validate-phase.sh"),
            "#!/bin/bash\n# ported to python",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("agent-metrics.sh"),
            "#!/bin/bash\n# ported to python",
        )
        .unwrap();
        fs::write(workspace.join(".loom").join("roles").join("builder.md"), "old builder").unwrap();
        fs::write(workspace.join(".loom").join("roles").join("obsolete.md"), "removed role")
            .unwrap();

        // Run reinstall
        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Stale scripts should be removed
        assert!(
            !workspace
                .join(".loom")
                .join("scripts")
                .join("validate-phase.sh")
                .exists(),
            "validate-phase.sh should have been removed"
        );
        assert!(
            !workspace
                .join(".loom")
                .join("scripts")
                .join("agent-metrics.sh")
                .exists(),
            "agent-metrics.sh should have been removed"
        );

        // Stale role should be removed
        assert!(
            !workspace
                .join(".loom")
                .join("roles")
                .join("obsolete.md")
                .exists(),
            "obsolete.md should have been removed"
        );

        // Current files should exist with fresh content
        let worktree =
            fs::read_to_string(workspace.join(".loom").join("scripts").join("worktree.sh"))
                .unwrap();
        assert_eq!(worktree, "#!/bin/bash\n# kept");

        let builder =
            fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
        assert_eq!(builder, "builder role");

        // Report should track removals
        assert!(report
            .removed
            .contains(&".loom/scripts/validate-phase.sh".to_string()));
        assert!(report
            .removed
            .contains(&".loom/scripts/agent-metrics.sh".to_string()));
        assert!(report
            .removed
            .contains(&".loom/roles/obsolete.md".to_string()));
    }

    #[test]
    fn test_init_report_includes_verification() {
        // Full initialization should include verification with no failures
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("scripts").join("test.sh"), "#!/bin/bash").unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Fresh install should have zero verification failures
        assert!(
            report.verification_failures.is_empty(),
            "Expected no verification failures, got: {:?}",
            report.verification_failures
        );
    }

    #[test]
    fn test_hooks_installed_on_fresh_install() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with hooks
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("hooks")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("hooks").join("guard-destructive.sh"), "#!/bin/bash\n# guard hook")
            .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Hook should be installed
        let hook_path = workspace
            .join(".loom")
            .join("hooks")
            .join("guard-destructive.sh");
        assert!(hook_path.exists(), "Hook file should be installed");
        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, "#!/bin/bash\n# guard hook");

        // Report should list the hook as added
        assert!(report
            .added
            .contains(&".loom/hooks/guard-destructive.sh".to_string()));
    }

    #[test]
    fn test_hooks_preserved_on_reinstall() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with hooks
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("hooks")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("hooks").join("guard-destructive.sh"),
            "#!/bin/bash\n# updated guard hook v2",
        )
        .unwrap();

        // Simulate existing installation with old hook
        fs::create_dir_all(workspace.join(".loom").join("hooks")).unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("hooks")
                .join("guard-destructive.sh"),
            "#!/bin/bash\n# old guard hook v1",
        )
        .unwrap();

        // Run reinstall
        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());

        // Hook should have new content (clean-then-copy)
        let hook_path = workspace
            .join(".loom")
            .join("hooks")
            .join("guard-destructive.sh");
        assert!(hook_path.exists(), "Hook file should exist after reinstall");
        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, "#!/bin/bash\n# updated guard hook v2");
    }

    #[test]
    fn test_scripts_lib_subdirectory_copied_on_fresh_install() {
        // Verifies that scripts/lib/ subdirectory (containing loom-tools.sh
        // and pipe-pane-cmd.sh) is correctly copied during initialization.
        // This is the specific scenario reported in issue #2392.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults mirroring real structure with scripts/lib/
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("scripts").join("agent-spawn.sh"),
            "#!/bin/bash\nsource \"$SCRIPT_DIR/lib/loom-tools.sh\"",
        )
        .unwrap();
        fs::write(
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
            "#!/bin/bash\n# shared helper library",
        )
        .unwrap();
        fs::write(
            defaults
                .join("scripts")
                .join("lib")
                .join("pipe-pane-cmd.sh"),
            "#!/bin/bash\n# pipe pane command",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // Verify scripts/lib/ subdirectory exists
        let lib_dir = workspace.join(".loom").join("scripts").join("lib");
        assert!(lib_dir.exists(), "scripts/lib/ directory should exist");
        assert!(lib_dir.is_dir(), "scripts/lib/ should be a directory");

        // Verify both lib files were copied
        let loom_tools = lib_dir.join("loom-tools.sh");
        assert!(loom_tools.exists(), "lib/loom-tools.sh should exist");
        let content = fs::read_to_string(&loom_tools).unwrap();
        assert_eq!(content, "#!/bin/bash\n# shared helper library");

        let pipe_pane = lib_dir.join("pipe-pane-cmd.sh");
        assert!(pipe_pane.exists(), "lib/pipe-pane-cmd.sh should exist");

        // Verify the parent script was also copied
        let agent_spawn = workspace
            .join(".loom")
            .join("scripts")
            .join("agent-spawn.sh");
        assert!(agent_spawn.exists(), "agent-spawn.sh should exist");

        // Verify report includes subdirectory files
        assert!(
            report
                .added
                .contains(&".loom/scripts/lib/loom-tools.sh".to_string()),
            "Report should include lib/loom-tools.sh, got: {:?}",
            report.added
        );
        assert!(
            report
                .added
                .contains(&".loom/scripts/lib/pipe-pane-cmd.sh".to_string()),
            "Report should include lib/pipe-pane-cmd.sh, got: {:?}",
            report.added
        );

        // Verify no verification failures
        assert!(
            report.verification_failures.is_empty(),
            "Expected no verification failures, got: {:?}",
            report.verification_failures
        );
    }

    #[test]
    fn test_scripts_lib_subdirectory_restored_on_reinstall() {
        // On reinstall, scripts/lib/ should be cleaned and re-copied.
        // This tests the case where lib/ existed but with stale content.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with scripts/lib/
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
            "#!/bin/bash\n# v2 helper",
        )
        .unwrap();

        // Simulate existing installation with old lib/ content and a stale file
        fs::create_dir_all(workspace.join(".loom").join("scripts").join("lib")).unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("lib")
                .join("loom-tools.sh"),
            "#!/bin/bash\n# v1 helper (old)",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("scripts")
                .join("lib")
                .join("obsolete.sh"),
            "#!/bin/bash\n# should be removed",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());
        let report = result.unwrap();

        // lib/loom-tools.sh should have new content
        let loom_tools = workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("loom-tools.sh");
        let content = fs::read_to_string(&loom_tools).unwrap();
        assert_eq!(content, "#!/bin/bash\n# v2 helper");

        // Stale file should be removed
        let obsolete = workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("obsolete.sh");
        assert!(!obsolete.exists(), "Stale file in lib/ should be removed on reinstall");

        // Report should track the removal
        assert!(
            report
                .removed
                .contains(&".loom/scripts/lib/obsolete.sh".to_string()),
            "Report should list obsolete.sh as removed, got: {:?}",
            report.removed
        );
    }

    #[test]
    fn test_docs_subdirectory_copied_on_fresh_install() {
        // Regression-guard for issue #3470: the `.loom/docs/` managed
        // directory (containing static reference docs like
        // `ci-integration.md` from issue #3333) must be copied during
        // initialization. The line-169 `sync_managed_dir(..., "docs", ...)`
        // call already exists on main — this test prevents the entry from
        // silently being deleted or refactored away in the future, which
        // would re-introduce the v0.10 field failure where consumers got
        // `MISSING: .loom/docs/ci-integration.md` from the post-install
        // metadata verification (the #3287 safety net).
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults mirroring the real shipped shape: docs live at
        // `defaults/docs/` alongside `defaults/roles/` (issue #3476 moved
        // them there from `defaults/.loom/docs/`, which `sync_managed_dir`
        // never looked at). The companion test
        // `test_real_defaults_tree_ships_docs_at_top_level` pins the actual
        // shipped tree to this layout so the two can't silently diverge.
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("docs")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults.join("docs").join("ci-integration.md"),
            "# CI Integration\n\nStatic reference documentation.",
        )
        .unwrap();
        // A second doc, to confirm we copy the whole directory and not
        // just one named file.
        fs::write(
            defaults.join("docs").join("troubleshooting.md"),
            "# Troubleshooting\n\nMore docs.",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok(), "init failed: {:?}", result.err());
        let report = result.unwrap();

        // The `.loom/docs/` directory must exist on disk post-init.
        let docs_dir = workspace.join(".loom").join("docs");
        assert!(docs_dir.exists(), ".loom/docs/ directory should exist");
        assert!(docs_dir.is_dir(), ".loom/docs/ should be a directory");

        // The specific file the field failure flagged must be present.
        let ci_md = docs_dir.join("ci-integration.md");
        assert!(
            ci_md.exists(),
            ".loom/docs/ci-integration.md should exist (this is the file the \
             v0.10 install regression reported as MISSING in issue #3470)"
        );
        let content = fs::read_to_string(&ci_md).unwrap();
        assert_eq!(content, "# CI Integration\n\nStatic reference documentation.");

        // The sibling doc must also be present (whole-directory copy).
        let troubleshooting = docs_dir.join("troubleshooting.md");
        assert!(troubleshooting.exists(), ".loom/docs/troubleshooting.md should exist");

        // Report bookkeeping: both docs files should be tracked as added.
        assert!(
            report
                .added
                .contains(&".loom/docs/ci-integration.md".to_string()),
            "Report should include docs/ci-integration.md, got: {:?}",
            report.added
        );
        assert!(
            report
                .added
                .contains(&".loom/docs/troubleshooting.md".to_string()),
            "Report should include docs/troubleshooting.md, got: {:?}",
            report.added
        );

        // No verification failures: the fail-fast assertion inside
        // sync_managed_dir (the #3220/#3287 safety net) should be quiet,
        // and the post-copy `verify_all_copied_files` walk over the docs
        // dir should not produce any content mismatches.
        assert!(
            report.verification_failures.is_empty(),
            "Expected no verification failures, got: {:?}",
            report.verification_failures
        );
    }

    #[test]
    fn test_docs_subdirectory_restored_on_reinstall() {
        // Reinstall analog of test_docs_subdirectory_copied_on_fresh_install:
        // the docs dir must be cleaned and re-copied so stale files are
        // removed and updated content lands. This pins the #3470 fix on
        // the reinstall path (which is the path the field failure
        // exercised — Studio was upgrading v0.9 -> v0.10).
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Defaults with updated docs.
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("docs")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("docs").join("ci-integration.md"), "# CI Integration v2").unwrap();

        // Pre-existing install with stale content + a stale file.
        fs::create_dir_all(workspace.join(".loom").join("docs")).unwrap();
        fs::write(
            workspace
                .join(".loom")
                .join("docs")
                .join("ci-integration.md"),
            "# CI Integration v1 (old)",
        )
        .unwrap();
        fs::write(workspace.join(".loom").join("docs").join("obsolete-doc.md"), "stale").unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok(), "reinstall failed: {:?}", result.err());
        let report = result.unwrap();

        // Updated file has new content.
        let ci_md = workspace
            .join(".loom")
            .join("docs")
            .join("ci-integration.md");
        let content = fs::read_to_string(&ci_md).unwrap();
        assert_eq!(content, "# CI Integration v2");

        // Stale file removed.
        let obsolete = workspace.join(".loom").join("docs").join("obsolete-doc.md");
        assert!(!obsolete.exists(), "Stale docs file should be removed on reinstall");
        assert!(
            report
                .removed
                .contains(&".loom/docs/obsolete-doc.md".to_string()),
            "Report should list obsolete-doc.md as removed, got: {:?}",
            report.removed
        );
    }

    #[test]
    fn test_real_defaults_tree_ships_docs_at_top_level() {
        // Regression guard for #3476 Bug 2. The tempdir tests above fabricate
        // a defaults/ layout, so they structurally cannot catch the failure
        // mode where the SHIPPED tree diverges from what `sync_managed_dir`
        // expects: v0.10.0 shipped docs at `defaults/.loom/docs/` while
        // `sync_managed_dir(&defaults, ..., "docs", ...)` resolved
        // `defaults/docs/`, so the copy silently no-oped and real installs
        // failed the #3287 metadata check with
        // `MISSING: .loom/docs/ci-integration.md`.
        //
        // This test runs against the actual repository tree (resolved via
        // CARGO_MANIFEST_DIR) and asserts every managed dir that
        // `initialize_workspace` syncs — including docs — exists at the
        // top level of defaults/ where `sync_managed_dir` will find it.
        let defaults = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("loom-daemon/ has a parent")
            .join("defaults");
        assert!(
            defaults.is_dir(),
            "shipped defaults/ tree not found at {defaults:?} — did the repo layout change?"
        );

        for dir_name in &["roles", "scripts", "hooks", "docs"] {
            let managed = defaults.join(dir_name);
            assert!(
                managed.is_dir(),
                "defaults/{dir_name}/ is missing — sync_managed_dir(\"{dir_name}\") would \
                 silently no-op and installs would diverge from the manifest (#3476)"
            );
        }

        // The specific file the field failure flagged.
        assert!(
            defaults.join("docs").join("ci-integration.md").is_file(),
            "defaults/docs/ci-integration.md missing — the #3287 metadata guard would \
             report it MISSING on every install (#3476)"
        );

        // The old nested location must stay gone: a file reappearing at
        // defaults/.loom/docs/ would be manifest-listed (via the `.loom/*`
        // literal rule in scripts/install/manifest.sh) but never copied by
        // sync_managed_dir — the exact divergence this issue fixed.
        assert!(
            !defaults.join(".loom").join("docs").exists(),
            "defaults/.loom/docs/ has reappeared — docs must live at defaults/docs/ \
             so sync_managed_dir copies them (#3476)"
        );
    }

    #[test]
    fn test_filter_preserved_from_verification_failures_removes_preserved() {
        // Files preserved by merge strategy must not appear as verification failures
        // (this is the regression case from issue #3218).
        let mut report = InitReport {
            preserved: vec![
                ".claude/settings.json".to_string(),
                ".github/labels.yml".to_string(),
            ],
            verification_failures: vec![
                ".claude/settings.json (content mismatch: source 100 bytes, installed 200 bytes)"
                    .to_string(),
                ".github/labels.yml (content mismatch: source 50 bytes, installed 75 bytes)"
                    .to_string(),
                ".loom/scripts/genuine.sh (content mismatch: source 10 bytes, installed 20 bytes)"
                    .to_string(),
            ],
            ..Default::default()
        };

        filter_preserved_from_verification_failures(&mut report);

        // Only the genuine non-preserved failure should remain
        assert_eq!(report.verification_failures.len(), 1);
        assert!(report.verification_failures[0].contains(".loom/scripts/genuine.sh"));
    }

    #[test]
    fn test_filter_preserved_from_verification_failures_no_preserved() {
        // When nothing is preserved, all failures pass through unchanged
        let mut report = InitReport {
            preserved: vec![],
            verification_failures: vec![".loom/scripts/foo.sh (content mismatch)".to_string()],
            ..Default::default()
        };
        filter_preserved_from_verification_failures(&mut report);
        assert_eq!(report.verification_failures.len(), 1);
    }

    #[test]
    fn test_filter_preserved_from_verification_failures_no_failures() {
        // No-op when there are no failures, even if there are preserved files
        let mut report = InitReport {
            preserved: vec![".claude/settings.json".to_string()],
            verification_failures: vec![],
            ..Default::default()
        };
        filter_preserved_from_verification_failures(&mut report);
        assert!(report.verification_failures.is_empty());
    }

    #[test]
    fn test_preserved_files_excluded_from_verification_failures_end_to_end() {
        // End-to-end: a customized .github/labels.yml (preserved by merge_dir_with_report
        // on reinstall) should not appear as a verification failure after init, even
        // though its bytes differ from the source defaults. This is the regression case
        // from issue #3218.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Minimal defaults
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();

        // .github scaffolding default with content X
        fs::create_dir_all(defaults.join(".github")).unwrap();
        fs::write(defaults.join(".github").join("labels.yml"), "- name: default\n  color: ffffff")
            .unwrap();

        // Pre-existing customized .github/labels.yml with content Y (different bytes)
        fs::create_dir_all(workspace.join(".github")).unwrap();
        fs::write(
            workspace.join(".github").join("labels.yml"),
            "- name: customized\n  color: 000000\n  description: user edit",
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok());
        let report = result.unwrap();

        // The customized file must be reported as preserved
        assert!(
            report.preserved.contains(&".github/labels.yml".to_string()),
            "preserved should contain .github/labels.yml, got: {:?}",
            report.preserved
        );

        // And it must NOT appear as a verification failure (the bug we're fixing)
        let leaked: Vec<&String> = report
            .verification_failures
            .iter()
            .filter(|f| f.contains(".github/labels.yml"))
            .collect();
        assert!(
            leaked.is_empty(),
            "preserved file leaked into verification_failures: {:?}",
            report.verification_failures
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_scripts_made_executable_including_subdirectories() {
        // Verifies that make_shell_scripts_executable works recursively
        // on scripts/ and its subdirectories (e.g., scripts/lib/).
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with scripts that are NOT executable
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(defaults.join("scripts").join("worktree.sh"), "#!/bin/bash\n# worktree helper")
            .unwrap();
        fs::write(
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
            "#!/bin/bash\n# shared helper",
        )
        .unwrap();

        // Remove execute bit from source files to simulate git clone stripping perms
        for path in &[
            defaults.join("scripts").join("worktree.sh"),
            defaults.join("scripts").join("lib").join("loom-tools.sh"),
        ] {
            let metadata = fs::metadata(path).unwrap();
            let mut perms = metadata.permissions();
            perms.set_mode(0o644); // rw-r--r-- (no execute)
            fs::set_permissions(path, perms).unwrap();
        }

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

        assert!(result.is_ok());

        // Both scripts should be executable after init
        let worktree_sh = workspace.join(".loom").join("scripts").join("worktree.sh");
        let perms = fs::metadata(&worktree_sh).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "worktree.sh should be executable, mode: {:o}",
            perms.mode()
        );

        let loom_tools = workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("loom-tools.sh");
        let perms = fs::metadata(&loom_tools).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "lib/loom-tools.sh should be executable, mode: {:o}",
            perms.mode()
        );
    }

    #[test]
    fn test_find_missing_files_empty_when_all_present() {
        // Regression test for issue #3220: the post-copy assertion in
        // sync_managed_dir uses find_missing_files to detect partial copies.
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(dst.join("lib")).unwrap();
        fs::write(src.join("a.sh"), "a").unwrap();
        fs::write(dst.join("a.sh"), "a").unwrap();
        fs::write(src.join("lib").join("b.sh"), "b").unwrap();
        fs::write(dst.join("lib").join("b.sh"), "b").unwrap();

        let missing = find_missing_files(&src, &dst);
        assert!(missing.is_empty(), "Expected no missing files, got: {missing:?}");
    }

    #[test]
    fn test_find_missing_files_detects_missing_subdirectory_file() {
        // Specifically verifies the issue #3220 scenario: a file in a
        // subdirectory (e.g., scripts/lib/forge-helpers.sh) is missing
        // from the destination.
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(dst.join("lib")).unwrap();
        fs::write(src.join("a.sh"), "a").unwrap();
        fs::write(dst.join("a.sh"), "a").unwrap();
        fs::write(src.join("lib").join("loom-tools.sh"), "tools").unwrap();
        fs::write(dst.join("lib").join("loom-tools.sh"), "tools").unwrap();
        // Source has forge-helpers.sh but destination does NOT — this is
        // the exact failure mode from issue #3220.
        fs::write(src.join("lib").join("forge-helpers.sh"), "helpers").unwrap();

        let missing = find_missing_files(&src, &dst);
        assert_eq!(missing.len(), 1, "Expected 1 missing file, got: {missing:?}");
        assert_eq!(missing[0], "lib/forge-helpers.sh");
    }

    #[test]
    fn test_find_missing_files_detects_entire_missing_subdir() {
        // If a whole subdirectory is missing, every file under it should be reported.
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("lib").join("a.sh"), "a").unwrap();
        fs::write(src.join("lib").join("b.sh"), "b").unwrap();

        let missing = find_missing_files(&src, &dst);
        assert_eq!(missing.len(), 2, "Expected 2 missing files, got: {missing:?}");
        let mut sorted = missing;
        sorted.sort();
        assert_eq!(sorted, vec!["lib/a.sh".to_string(), "lib/b.sh".to_string()]);
    }

    // ------------------------------------------------------------------
    // config.json merge (issue #3598)
    // ------------------------------------------------------------------

    /// Write a minimal defaults/ tree with the given config.json body and
    /// return (workspace, defaults) paths for `initialize_workspace`.
    fn setup_config_merge_repo(
        temp: &TempDir,
        template: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let workspace = temp.path().to_path_buf();
        let defaults = workspace.join("defaults");
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::write(defaults.join("config.json"), template).unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        (workspace, defaults)
    }

    #[test]
    fn test_config_worktree_root_survives_reinstall() {
        // The core issue #3598 repro: a committed config.json with a
        // worktree.root override must retain that key after reinstall.
        let temp = TempDir::new().unwrap();
        let template = r#"{"version": "2", "offlineMode": false}"#;
        let (workspace, defaults) = setup_config_merge_repo(&temp, template);

        // Pre-existing consumer config carrying a worktree.root override.
        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("config.json"),
            r#"{"version": "2", "worktree": {"root": "/Volumes/Stripe"}}"#,
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok(), "init failed: {result:?}");
        let report = result.unwrap();

        let merged: Value = serde_json::from_str(
            &fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
        )
        .unwrap();

        // Consumer override preserved...
        assert_eq!(merged["worktree"]["root"], Value::String("/Volumes/Stripe".to_string()));
        // ...and a template key absent from the consumer file was added.
        assert_eq!(merged["offlineMode"], Value::Bool(false));

        // Merged (not clobbered) → reported as preserved so verification stays green.
        assert!(
            report.preserved.contains(&".loom/config.json".to_string()),
            "config.json should be reported preserved, got: {:?}",
            report.preserved
        );
    }

    #[test]
    fn test_config_deep_merge_preserves_unknown_keys_and_conflict_resolution() {
        // Deep merge at any depth: unknown consumer keys survive, and on a
        // key present in BOTH files the consumer value wins while new template
        // keys are still delivered.
        let temp = TempDir::new().unwrap();
        let template = r#"{
          "version": "2",
          "reflection": {"enabled": true, "categories": ["bug", "enhancement"]},
          "newTemplateKey": "shipped"
        }"#;
        let (workspace, defaults) = setup_config_merge_repo(&temp, template);

        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("config.json"),
            r#"{
              "version": "2",
              "reflection": {"enabled": false, "upstream_repo": "me/fork"},
              "worktree": {"root": "/Volumes/X"},
              "customConsumerKey": {"nested": [1, 2, 3]}
            }"#,
        )
        .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok(), "init failed: {result:?}");

        let merged: Value = serde_json::from_str(
            &fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
        )
        .unwrap();

        // Conflict on reflection.enabled → consumer (false) wins.
        assert_eq!(merged["reflection"]["enabled"], Value::Bool(false));
        // Consumer-only nested key preserved.
        assert_eq!(merged["reflection"]["upstream_repo"], Value::String("me/fork".to_string()));
        // Template-only nested key delivered.
        assert_eq!(merged["reflection"]["categories"], serde_json::json!(["bug", "enhancement"]));
        // New top-level template key delivered.
        assert_eq!(merged["newTemplateKey"], Value::String("shipped".to_string()));
        // Unknown consumer keys (including deeply nested arrays) preserved.
        assert_eq!(merged["worktree"]["root"], Value::String("/Volumes/X".to_string()));
        assert_eq!(merged["customConsumerKey"]["nested"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_config_merge_is_idempotent_across_repeat_reinstalls() {
        // A second consecutive reinstall must leave config.json byte-identical
        // (same bar as the #3590 .gitignore fix).
        let temp = TempDir::new().unwrap();
        let template = r#"{"version": "2", "offlineMode": false, "terminals": []}"#;
        let (workspace, defaults) = setup_config_merge_repo(&temp, template);

        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("config.json"),
            r#"{"version": "2", "worktree": {"root": "/Volumes/Stripe"}}"#,
        )
        .unwrap();

        let config_path = workspace.join(".loom").join("config.json");

        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("first reinstall");
        let after_first = fs::read_to_string(&config_path).unwrap();

        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("second reinstall");
        let after_second = fs::read_to_string(&config_path).unwrap();

        assert_eq!(
            after_first, after_second,
            "config.json must be byte-identical across repeat reinstalls"
        );
        // The override still survives the second pass.
        let merged: Value = serde_json::from_str(&after_second).unwrap();
        assert_eq!(merged["worktree"]["root"], Value::String("/Volumes/Stripe".to_string()));
    }

    #[test]
    fn test_config_fresh_install_is_exact_template_copy() {
        // No existing .loom/config.json → exact byte-for-byte template copy,
        // reported as added (fresh-install behavior unchanged).
        let temp = TempDir::new().unwrap();
        let template = "{\n  \"version\": \"2\",\n  \"offlineMode\": false\n}\n";
        let (workspace, defaults) = setup_config_merge_repo(&temp, template);

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok(), "init failed: {result:?}");
        let report = result.unwrap();

        let installed = fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap();
        assert_eq!(installed, template, "fresh install must be an exact template copy");
        assert!(
            report.added.contains(&".loom/config.json".to_string()),
            "fresh config.json should be reported added, got: {:?}",
            report.added
        );
    }

    #[test]
    fn test_config_fresh_install_and_reinstall_are_byte_identical() {
        // Issue #3619: the fresh-install write path and the reinstall-merge
        // write path must emit BYTE-IDENTICAL output for the same logical
        // content. Before the fix, fresh install did a raw `fs::copy` of the
        // hand-formatted template (semantic key order, inline arrays) while the
        // reinstall merge re-serialized via `to_string_pretty` (expanded
        // arrays), so the first reinstall reformatted config.json and left it
        // permanently dirty. Now both paths serialize, so a fresh install
        // followed by a reinstall is a byte-for-byte no-op.
        let temp = TempDir::new().unwrap();
        // A template exercising the two axes that used to diverge: an inline
        // array (expanded by to_string_pretty) and multiple keys in a
        // non-alphabetical semantic order (preserved by `preserve_order`).
        let template = r#"{
  "version": "2",
  "offlineMode": false,
  "reflection": {
    "enabled": true,
    "categories": ["bug", "enhancement", "documentation"]
  },
  "terminals": []
}"#;
        let (workspace, defaults) = setup_config_merge_repo(&temp, template);
        let config_path = workspace.join(".loom").join("config.json");

        // Fresh install (no existing .loom/config.json).
        let first =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
                .expect("fresh install");
        assert!(
            first.added.contains(&".loom/config.json".to_string()),
            "fresh install should report config.json as added, got: {:?}",
            first.added
        );
        let after_fresh = fs::read_to_string(&config_path).unwrap();

        // Second run: now the file exists → the merge path runs. Its output
        // must be byte-identical to the fresh-install output (the crux of
        // #3619 — a reinstall over a freshly-installed config leaves it clean).
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("reinstall merge");
        let after_reinstall = fs::read_to_string(&config_path).unwrap();

        assert_eq!(
            after_fresh, after_reinstall,
            "fresh-install and reinstall-merge output must be byte-identical (#3619)"
        );

        // Sanity: the serialized form is canonical (expanded array, trailing
        // newline, template key order preserved by `preserve_order`).
        assert!(
            after_fresh.ends_with("}\n"),
            "serialized config.json should end with a single trailing newline"
        );
        let version_pos = after_fresh.find("\"version\"").unwrap();
        let offline_pos = after_fresh.find("\"offlineMode\"").unwrap();
        assert!(
            version_pos < offline_pos,
            "preserve_order must retain template key order (version before offlineMode)"
        );
    }

    #[test]
    fn test_config_invalid_existing_json_falls_back_to_template() {
        // A corrupt existing config.json falls back to the template copy with a
        // warning (does not abort) and is recorded as updated.
        let temp = TempDir::new().unwrap();
        let template = r#"{"version": "2", "offlineMode": false}"#;
        let (workspace, defaults) = setup_config_merge_repo(&temp, template);

        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(workspace.join(".loom").join("config.json"), "{ this is not valid json ,,,")
            .unwrap();

        let result =
            initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
        assert!(result.is_ok(), "init must not abort on invalid config: {result:?}");
        let report = result.unwrap();

        // The template must have replaced the corrupt file (valid JSON now).
        let installed: Value = serde_json::from_str(
            &fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
        )
        .expect("post-fallback config.json must be valid JSON");
        assert_eq!(installed["offlineMode"], Value::Bool(false));
        assert!(
            report.updated.contains(&".loom/config.json".to_string()),
            "fallback config.json should be reported updated, got: {:?}",
            report.updated
        );
    }

    #[test]
    fn test_deep_merge_existing_wins_unit() {
        // Direct unit coverage of the merge primitive.
        let mut base = serde_json::json!({
            "a": 1,
            "shared": {"x": "template", "onlyTemplate": true},
            "arr": [1, 2]
        });
        let overlay = serde_json::json!({
            "shared": {"x": "consumer", "onlyConsumer": 9},
            "arr": [9],
            "b": 2
        });
        deep_merge_existing_wins(&mut base, &overlay);

        // Template-only top-level key retained.
        assert_eq!(base["a"], serde_json::json!(1));
        // Overlay-only top-level key added.
        assert_eq!(base["b"], serde_json::json!(2));
        // Nested object merged; overlay wins on conflict, both-only keys kept.
        assert_eq!(base["shared"]["x"], serde_json::json!("consumer"));
        assert_eq!(base["shared"]["onlyTemplate"], serde_json::json!(true));
        assert_eq!(base["shared"]["onlyConsumer"], serde_json::json!(9));
        // Arrays are replaced wholesale by the overlay (non-object value).
        assert_eq!(base["arr"], serde_json::json!([9]));
    }
}
