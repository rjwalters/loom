//! Repository scaffolding setup
//!
//! Sets up CLAUDE.md, .claude/, .codex/, and .github/ directories.

use std::fs;
use std::path::Path;

use super::file_ops::{copy_dir_with_report, force_merge_dir_with_report, merge_dir_with_report};
use super::git::extract_repo_info;
use super::templates::{substitute_template_variables, LoomMetadata};
use super::InitReport;

/// Loom section markers for CLAUDE.md content preservation
pub const LOOM_SECTION_START: &str = "<!-- BEGIN LOOM ORCHESTRATION -->";
pub const LOOM_SECTION_END: &str = "<!-- END LOOM ORCHESTRATION -->";

/// The short pointer injected into root CLAUDE.md (between section markers).
///
/// The full Loom guide is written to `.loom/CLAUDE.md` in the target repo.
/// Claude Code auto-discovers `.loom/CLAUDE.md` when agents work in
/// `.loom/worktrees/issue-N/` via ancestor directory traversal.
pub const LOOM_ROOT_POINTER: &str = "This repository uses [Loom](https://github.com/rjwalters/loom) for AI-powered development orchestration. See `.loom/CLAUDE.md` for the full guide (roles, labels, worktrees, configuration).";

/// Wrap Loom content in section markers
pub fn wrap_loom_content(content: &str) -> String {
    format!("{}\n{}\n{}", LOOM_SECTION_START, content.trim(), LOOM_SECTION_END)
}

/// Setup repository scaffolding files
///
/// Copies CLAUDE.md, .claude/, .codex/, and .github/ to the workspace.
/// - Fresh install: Copies all files from defaults
/// - Reinstall without force (merge mode): Adds new files, preserves ALL existing files
/// - Reinstall with force (force-merge mode): Updates default files, preserves custom files
/// - Template variables: Substitutes variables in CLAUDE.md and workflow files
///   - `{{REPO_OWNER}}`, `{{REPO_NAME}}`: Repository info from git remote
///   - `{{LOOM_VERSION}}`, `{{LOOM_COMMIT}}`, `{{INSTALL_DATE}}`: Loom installation metadata
///
/// **CLAUDE.md Handling**:
/// - Full Loom guide is written to `<workspace>/.loom/CLAUDE.md` (with template substitution)
/// - Only a short pointer is injected into root `CLAUDE.md` (between Loom section markers)
/// - If existing root CLAUDE.md has Loom section markers, only the marked section is replaced
/// - If existing root CLAUDE.md has no markers, Loom pointer is appended at the end
/// - All existing root CLAUDE.md content is preserved exactly as-is
/// - Claude Code auto-discovers `.loom/CLAUDE.md` in `.loom/worktrees/issue-N/` via ancestor dirs
///
/// Custom files (files in workspace that don't exist in defaults) are always preserved.
#[allow(clippy::too_many_lines)]
pub fn setup_repository_scaffolding(
    workspace_path: &Path,
    defaults_path: &Path,
    force: bool,
    report: &mut InitReport,
) -> Result<(), String> {
    // Extract repository owner and name for template substitution
    let repo_info = extract_repo_info(workspace_path);
    let (repo_owner, repo_name) = match repo_info {
        Some((owner, name)) => (Some(owner), Some(name)),
        None => (None, None),
    };

    // Get Loom installation metadata from environment variables
    let loom_metadata = LoomMetadata::from_env();

    // Helper to copy directory with force logic and reporting
    // - Fresh install (dst doesn't exist): copy all
    // - Reinstall without force: merge (add new, preserve existing)
    // - Reinstall with force: force-merge (update defaults, preserve custom)
    let copy_directory =
        |src: &Path, dst: &Path, name: &str, report: &mut InitReport| -> Result<(), String> {
            if src.exists() {
                if !dst.exists() {
                    // Fresh install: copy all
                    copy_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to copy {name}: {e}"))?;
                } else if force {
                    // Force reinstall: update defaults, preserve custom files
                    force_merge_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to force-merge {name}: {e}"))?;
                } else {
                    // Merge reinstall: add new files only, preserve all existing
                    merge_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to merge {name}: {e}"))?;
                }
            }
            Ok(())
        };

    // Handle Loom CLAUDE.md content:
    //
    // 1. Write full Loom guide to `<workspace>/.loom/CLAUDE.md` (template substituted)
    //    - Claude Code discovers this automatically when agents work in worktrees
    //    - Always written on install/reinstall (overwrite on reinstall to get latest content)
    //
    // 2. Inject short pointer into root `CLAUDE.md` (between Loom section markers)
    //    - Keeps root CLAUDE.md minimal — saves context budget for non-Loom sessions
    //    - If existing root has markers, only the marked section is replaced
    //    - If existing root has no markers, pointer is appended at the end
    let claude_md_src = defaults_path.join(".loom").join("CLAUDE.md");

    if claude_md_src.exists() {
        // Read the Loom template content
        let loom_content = fs::read_to_string(&claude_md_src)
            .map_err(|e| format!("Failed to read CLAUDE.md template: {e}"))?;

        // Substitute template variables in Loom content
        let loom_substituted = substitute_template_variables(
            &loom_content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        // --- Step 1: Write full guide to .loom/CLAUDE.md ---
        let loom_dir = workspace_path.join(".loom");
        // .loom/ should already exist (created earlier in initialize_workspace),
        // but create it if it doesn't to be safe.
        if !loom_dir.exists() {
            fs::create_dir_all(&loom_dir)
                .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
        }
        let loom_claude_md_dst = loom_dir.join("CLAUDE.md");
        let loom_claude_md_existed = loom_claude_md_dst.exists();
        fs::write(&loom_claude_md_dst, &loom_substituted)
            .map_err(|e| format!("Failed to write .loom/CLAUDE.md: {e}"))?;
        if loom_claude_md_existed {
            report.updated.push(".loom/CLAUDE.md".to_string());
        } else {
            report.added.push(".loom/CLAUDE.md".to_string());
        }

        // --- Step 2: Inject short pointer into root CLAUDE.md ---
        let claude_md_dst = workspace_path.join("CLAUDE.md");
        let existed = claude_md_dst.exists();

        // The pointer is a single-line description wrapped in section markers
        let wrapped_pointer = wrap_loom_content(LOOM_ROOT_POINTER);

        let final_content = if existed {
            // Read existing content
            let existing_content = fs::read_to_string(&claude_md_dst)
                .map_err(|e| format!("Failed to read existing CLAUDE.md: {e}"))?;

            // Check if existing file already has Loom section markers
            if existing_content.contains(LOOM_SECTION_START) {
                // Replace just the Loom section with the pointer, preserve everything else
                if let (Some(start_idx), Some(end_idx)) = (
                    existing_content.find(LOOM_SECTION_START),
                    existing_content.find(LOOM_SECTION_END),
                ) {
                    let before = &existing_content[..start_idx];
                    let after_end = end_idx + LOOM_SECTION_END.len();
                    let after = if after_end < existing_content.len() {
                        &existing_content[after_end..]
                    } else {
                        ""
                    };

                    format!("{}{}{}", before.trim_end(), wrapped_pointer, after)
                } else {
                    // Malformed markers - append pointer at end
                    format!("{}\n\n{}", existing_content.trim(), wrapped_pointer)
                }
            } else {
                // No markers exist - append Loom pointer at end
                format!("{}\n\n{}", existing_content.trim(), wrapped_pointer)
            }
        } else {
            // New file - just use wrapped pointer
            wrapped_pointer
        };

        // Only write if we're creating new or content changed
        if existed {
            let current = fs::read_to_string(&claude_md_dst).unwrap_or_default();
            if final_content != current {
                fs::write(&claude_md_dst, &final_content)
                    .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
                if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                    report.updated.push("CLAUDE.md".to_string());
                }
            } else if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                report.preserved.push("CLAUDE.md".to_string());
            }
        } else {
            fs::write(&claude_md_dst, &final_content)
                .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
            report.added.push("CLAUDE.md".to_string());
        }
    }

    // Copy .claude/ directory - always update default commands, preserve custom commands
    // - Fresh install: copy all from defaults
    // - Reinstall: always force-merge (update defaults, preserve custom)
    //
    // This ensures command updates from loom propagate to target repos while
    // preserving any custom commands the project has added.
    // Consistent with .loom/roles/ and .loom/scripts/ behavior.
    let claude_src = defaults_path.join(".claude");
    let claude_dst = workspace_path.join(".claude");
    if claude_src.exists() {
        if claude_dst.exists() {
            // Reinstall: always force-merge to update default commands
            // Custom commands (files not in defaults) are preserved
            force_merge_dir_with_report(&claude_src, &claude_dst, ".claude", report)
                .map_err(|e| format!("Failed to force-merge .claude directory: {e}"))?;
        } else {
            // Fresh install: copy all
            copy_dir_with_report(&claude_src, &claude_dst, ".claude", report)
                .map_err(|e| format!("Failed to copy .claude directory: {e}"))?;
        }
    }

    // Copy .codex/ directory
    copy_directory(
        &defaults_path.join(".codex"),
        &workspace_path.join(".codex"),
        ".codex",
        report,
    )?;

    // Copy .github/ directory
    copy_directory(
        &defaults_path.join(".github"),
        &workspace_path.join(".github"),
        ".github",
        report,
    )?;

    // Process workflow files with template variable substitution
    let workflow_file = workspace_path
        .join(".github")
        .join("workflows")
        .join("label-external-issues.yml");

    if workflow_file.exists() {
        let content = fs::read_to_string(&workflow_file)
            .map_err(|e| format!("Failed to read workflow file: {e}"))?;

        let substituted = substitute_template_variables(
            &content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        fs::write(&workflow_file, substituted)
            .map_err(|e| format!("Failed to write workflow file: {e}"))?;
    }

    // Note: scripts/ is now copied earlier in initialize_workspace()
    // to .loom/scripts/ along with other .loom-specific files

    // Copy package.json ONLY if workspace doesn't have one
    // (never overwrite existing package.json, even in force mode)
    // This provides stub scripts for pnpm commands referenced in roles
    let package_json_src = defaults_path.join("package.json");
    let package_json_dst = workspace_path.join("package.json");
    if package_json_src.exists() && !package_json_dst.exists() {
        fs::copy(&package_json_src, &package_json_dst)
            .map_err(|e| format!("Failed to copy package.json: {e}"))?;
    }

    // Install loom.sh convenience wrapper at repo root (always update from defaults)
    // This is a thin wrapper around .loom/scripts/daemon.sh that lets
    // users run `./loom.sh` from the repo root instead of the full script path.
    let loom_sh_src = defaults_path.join("loom.sh");
    let loom_sh_dst = workspace_path.join("loom.sh");
    if loom_sh_src.exists() {
        fs::copy(&loom_sh_src, &loom_sh_dst).map_err(|e| format!("Failed to copy loom.sh: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&loom_sh_dst)
                .map_err(|e| format!("Failed to read loom.sh metadata: {e}"))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&loom_sh_dst, perms)
                .map_err(|e| format!("Failed to make loom.sh executable: {e}"))?;
        }
        report.updated.push("loom.sh".to_string());
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_wrap_loom_content() {
        let content = "# Loom Orchestration\n\nLoom content here.";
        let wrapped = wrap_loom_content(content);

        assert!(wrapped.starts_with(LOOM_SECTION_START));
        assert!(wrapped.ends_with(LOOM_SECTION_END));
        assert!(wrapped.contains("Loom content here"));
    }

    #[test]
    fn test_setup_repository_scaffolding_force_mode() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults directory with .claude commands
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("loom.md"),
            "loom command from defaults",
        )
        .unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("builder.md"),
            "builder command from defaults",
        )
        .unwrap();

        // Create existing .claude directory in workspace with custom commands
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("custom.md"),
            "my custom command",
        )
        .unwrap();
        fs::write(workspace.join(".claude").join("commands").join("loom.md"), "old loom command")
            .unwrap();

        // Run setup with force=true (force-merge mode)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify custom.md was PRESERVED (custom file not in defaults)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());
        let custom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md"))
                .unwrap();
        assert_eq!(custom_content, "my custom command");

        // Verify loom.md was UPDATED with new content (default file)
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md was ADDED (new file from defaults)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("builder.md")
            .exists());
    }

    #[test]
    fn test_setup_repository_scaffolding_merge_mode() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults directory with .claude commands
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("loom.md"),
            "loom command from defaults",
        )
        .unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("builder.md"),
            "builder command from defaults",
        )
        .unwrap();

        // Create existing .claude directory in workspace with custom commands
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("custom.md"),
            "my custom command",
        )
        .unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("loom.md"),
            "custom loom command",
        )
        .unwrap();

        // Run setup with force=false (merge mode for .codex/.github, but .claude/ always force-merges)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify custom.md still exists (preserved)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());
        let custom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md"))
                .unwrap();
        assert_eq!(custom_content, "my custom command");

        // Verify loom.md was UPDATED with new content (default file)
        // .claude/ always force-merges on reinstall to propagate command updates
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md was added (new file)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("builder.md")
            .exists());
        let builder_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("builder.md"),
        )
        .unwrap();
        assert_eq!(builder_content, "builder command from defaults");
    }

    #[test]
    fn test_package_json_copied_when_missing() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with package.json
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join("package.json"),
            r#"{"name": "loom-workspace", "scripts": {"test": "echo test"}}"#,
        )
        .unwrap();

        // Workspace has no package.json initially
        assert!(!workspace.join("package.json").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify package.json was copied
        assert!(workspace.join("package.json").exists());
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("loom-workspace"));
    }

    #[test]
    fn test_package_json_preserved_when_exists() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with package.json
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join("package.json"),
            r#"{"name": "loom-workspace", "scripts": {"test": "echo test"}}"#,
        )
        .unwrap();

        // Create existing package.json in workspace (project-specific)
        fs::write(
            workspace.join("package.json"),
            r#"{"name": "my-rust-project", "scripts": {"build": "cargo build"}}"#,
        )
        .unwrap();

        // Run setup with force=true (should STILL preserve package.json)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify package.json was NOT overwritten
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("my-rust-project"));
        assert!(!content.contains("loom-workspace"));
    }

    /// Helper to create a standard test setup with a CLAUDE.md template in defaults
    fn setup_test_with_claude_template(
        temp_dir: &TempDir,
        template_content: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let workspace = temp_dir.path().to_path_buf();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(defaults.join(".loom").join("CLAUDE.md"), template_content).unwrap();

        (workspace, defaults)
    }

    #[test]
    fn test_loom_claude_md_written_to_loom_dir() {
        // Verifies full content goes to .loom/CLAUDE.md on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content here.",
        );

        // Pre-create .loom/ dir (as initialize_workspace normally does)
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        // Verify .loom/CLAUDE.md was created with full guide content
        assert!(workspace.join(".loom").join("CLAUDE.md").exists());
        let loom_claude_content =
            fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_claude_content.contains("Loom Orchestration - Repository Guide"));
        assert!(loom_claude_content.contains("Full guide content here"));
        assert!(report.added.contains(&".loom/CLAUDE.md".to_string()));
    }

    #[test]
    fn test_root_claude_md_contains_only_pointer() {
        // Verifies root CLAUDE.md has short pointer, not full guide, on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content here.",
        );

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // No existing root CLAUDE.md
        assert!(!workspace.join("CLAUDE.md").exists());

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        // Verify root CLAUDE.md has only the pointer, not the full guide
        assert!(workspace.join("CLAUDE.md").exists());
        let root_content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(root_content.contains(LOOM_SECTION_START));
        assert!(root_content.contains(LOOM_SECTION_END));
        assert!(root_content.contains(LOOM_ROOT_POINTER));
        // Full guide content must NOT be in root CLAUDE.md
        assert!(!root_content.contains("Full guide content here"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));
    }

    #[test]
    fn test_claude_md_preservation_new_install() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nLoom content here.",
        )
        .unwrap();

        // Pre-create .loom/ dir
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // No existing root CLAUDE.md in workspace
        assert!(!workspace.join("CLAUDE.md").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify root CLAUDE.md was created with section markers and short pointer only
        assert!(workspace.join("CLAUDE.md").exists());
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full content must be absent from root
        assert!(!content.contains("Loom content here"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));

        // Verify .loom/CLAUDE.md was created with full content
        assert!(workspace.join(".loom").join("CLAUDE.md").exists());
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("Loom content here"));
    }

    #[test]
    fn test_claude_md_preservation_existing_project_content() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nNew Loom content.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md with project-specific content (no markers)
        fs::write(
            workspace.join("CLAUDE.md"),
            r"# My Awesome Project

This project does amazing things with Rust.

## Getting Started

Run `cargo run` to start.",
        )
        .unwrap();

        // Run setup - Loom pointer should be appended at end
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify existing content was preserved and Loom pointer appended
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Awesome Project"));
        assert!(content.contains("amazing things with Rust"));
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full Loom guide must NOT be in root
        assert!(!content.contains("New Loom content"));

        // Project content should come BEFORE Loom section (appended at end)
        let project_pos = content.find("My Awesome Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // No duplicate content
        assert_eq!(content.matches("My Awesome Project").count(), 1);
    }

    #[test]
    fn test_claude_md_append_when_no_markers() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nLoom content here.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md WITHOUT markers (e.g., from previous install or manual creation)
        fs::write(
            workspace.join("CLAUDE.md"),
            r"# Lean Genius Project

Formal mathematics in Lean 4.

## Docker Build Safety

WARNING: Never run `lake build` inside Docker - causes memory corruption.

## Custom Agents

- Erdos: Mathematical proof orchestrator
- Aristotle: Automated theorem prover",
        )
        .unwrap();

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify existing content was preserved at top
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("Lean Genius Project"));
        assert!(content.contains("Docker Build Safety"));
        assert!(content.contains("Custom Agents"));

        // Verify Loom pointer was appended at end with markers
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full guide must NOT be in root
        assert!(!content.contains("Loom content here"));

        // Verify order: project content comes BEFORE Loom section
        let project_pos = content.find("Lean Genius Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // Verify no duplicate content or mangling
        assert_eq!(content.matches("Lean Genius Project").count(), 1);
    }

    #[test]
    fn test_claude_md_preservation_update_loom_section_only() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template (simulating upgrade)
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nUPDATED Loom content v2.0.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md with markers (previous install had full guide in root)
        // This simulates upgrading from old install where full guide was in root CLAUDE.md
        let existing = format!(
            "# My Project\n\nProject docs here.\n\n{LOOM_SECTION_START}\n# Loom Orchestration - Repository Guide\n\nOld Loom content v1.0.\n{LOOM_SECTION_END}"
        );
        fs::write(workspace.join("CLAUDE.md"), existing).unwrap();

        // Run setup with force=true
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify project content was preserved, Loom section was replaced with short pointer
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Project"));
        assert!(content.contains("Project docs here"));
        // Old full guide content must be gone from root
        assert!(!content.contains("Old Loom content v1.0"));
        // Updated full guide must also NOT be in root
        assert!(!content.contains("UPDATED Loom content v2.0"));
        // Root should now have the short pointer
        assert!(content.contains(LOOM_ROOT_POINTER));

        // Should only have ONE set of markers
        assert_eq!(
            content.matches(LOOM_SECTION_START).count(),
            1,
            "Should have exactly one start marker"
        );
        assert_eq!(
            content.matches(LOOM_SECTION_END).count(),
            1,
            "Should have exactly one end marker"
        );

        // Updated full guide content must be in .loom/CLAUDE.md
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("UPDATED Loom content v2.0"));
    }

    #[test]
    fn test_loom_claude_md_updated_on_reinstall() {
        // Verifies .loom/CLAUDE.md is overwritten on reinstall with new template content
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration\n\nUpdated content v2.",
        )
        .unwrap();

        // Pre-existing .loom/CLAUDE.md from previous install
        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration\n\nOld content v1.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify .loom/CLAUDE.md was updated with new content
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("Updated content v2"));
        assert!(!loom_content.contains("Old content v1"));
        assert!(report.updated.contains(&".loom/CLAUDE.md".to_string()));
    }

    #[test]
    fn test_claude_commands_always_updated_on_reinstall() {
        // .claude/ commands should always be force-merged on reinstall (without --force flag)
        // This ensures command updates propagate while custom commands are preserved
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with .claude commands
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("loom.md"),
            "loom command v2 with bug fix",
        )
        .unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("builder.md"),
            "builder command v2",
        )
        .unwrap();

        // Create existing .claude directory in workspace (simulates previous install)
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("loom.md"),
            "loom command v1 with bug",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("my-custom.md"),
            "my project-specific command",
        )
        .unwrap();

        // Run setup WITHOUT force flag (simulates normal reinstall)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify: loom.md was UPDATED (default command updated with bug fix)
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command v2 with bug fix");

        // Verify: builder.md was ADDED (new default command)
        let builder_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("builder.md"),
        )
        .unwrap();
        assert_eq!(builder_content, "builder command v2");

        // Verify: my-custom.md was PRESERVED (custom command not in defaults)
        let custom_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("my-custom.md"),
        )
        .unwrap();
        assert_eq!(custom_content, "my project-specific command");

        // Verify report reflects the changes
        assert!(report
            .updated
            .contains(&".claude/commands/loom.md".to_string()));
        assert!(report
            .added
            .contains(&".claude/commands/builder.md".to_string()));
        assert!(report
            .preserved
            .contains(&".claude/commands/my-custom.md".to_string()));
    }
}
