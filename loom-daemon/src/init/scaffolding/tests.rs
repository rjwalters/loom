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
fn test_load_internal_skip_list_missing_file() {
    // Missing skip-list file yields an empty set so existing repos that
    // ship without one keep their current behavior.
    let temp_dir = TempDir::new().unwrap();
    let set = load_internal_skip_list(temp_dir.path());
    assert!(set.is_empty());
}

#[test]
fn test_load_internal_skip_list_parses_entries() {
    // Issue #3464: confirm comment lines, blank lines, and surrounding
    // whitespace are handled correctly so the file is operator-editable
    // without surprise behavior.
    let temp_dir = TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(INTERNAL_SKIP_LIST_NAME),
        "# Loom-internal files\n\
             \n\
             .claude/commands/loom/release.md\n\
             \n\
             # second-section comment\n\
             .claude/commands/loom/some-other.md  \n",
    )
    .unwrap();

    let set = load_internal_skip_list(temp_dir.path());
    assert_eq!(set.len(), 2);
    assert!(set.contains(".claude/commands/loom/release.md"));
    assert!(set.contains(".claude/commands/loom/some-other.md"));
}

#[test]
fn test_setup_repository_scaffolding_skips_internal_files() {
    // End-to-end coverage of issue #3464: when defaults/.loom-internal.list
    // lists a path under .claude/, the installer must skip it on both
    // fresh install and reinstall, while sibling commands continue to
    // ship.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();

    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md"),
        "builder command",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("judge.md"),
        "judge command",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("release.md"),
        "loom-internal release skill",
    )
    .unwrap();

    // Skip-list excludes release.md.
    fs::write(
        defaults.join(INTERNAL_SKIP_LIST_NAME),
        "# header\n.claude/commands/loom/release.md\n",
    )
    .unwrap();

    let mut report = InitReport::default();
    setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

    // release.md must NOT be in the consumer's installed tree.
    assert!(
        !workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("release.md")
            .exists(),
        "issue #3464: .claude/commands/loom/release.md must be skipped on install"
    );
    // The siblings must still be installed verbatim.
    assert!(workspace
        .join(".claude")
        .join("commands")
        .join("loom")
        .join("builder.md")
        .exists());
    assert!(workspace
        .join(".claude")
        .join("commands")
        .join("loom")
        .join("judge.md")
        .exists());

    // Reinstall: same outcome, plus a stale local copy (if one exists)
    // is left in place rather than being overwritten or deleted.
    fs::write(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("release.md"),
        "stale local copy",
    )
    .unwrap();
    let mut report2 = InitReport::default();
    setup_repository_scaffolding(workspace, &defaults, true, &mut report2).unwrap();
    let local = fs::read_to_string(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("release.md"),
    )
    .unwrap();
    assert_eq!(
        local, "stale local copy",
        "skip rule must leave pre-existing local copies untouched"
    );
    // And the report must not list release.md as added/updated.
    assert!(!report2
        .added
        .iter()
        .chain(report2.updated.iter())
        .any(|p| p == ".claude/commands/loom/release.md"));
}

#[test]
fn test_setup_repository_scaffolding_force_mode() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults directory with .claude commands in loom/ subdirectory
    fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
        "loom command from defaults",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md"),
        "builder command from defaults",
    )
    .unwrap();

    // Create existing .claude directory in workspace with custom commands
    fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
    fs::write(
        workspace.join(".claude").join("commands").join("custom.md"),
        "my custom command",
    )
    .unwrap();
    fs::write(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
        "old loom command",
    )
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
        fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md")).unwrap();
    assert_eq!(custom_content, "my custom command");

    // Verify loom.md was UPDATED with new content (default file)
    let loom_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
    )
    .unwrap();
    assert_eq!(loom_content, "loom command from defaults");

    // Verify builder.md was ADDED (new file from defaults)
    assert!(workspace
        .join(".claude")
        .join("commands")
        .join("loom")
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

    // Create defaults directory with .claude commands in loom/ subdirectory
    fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
        "loom command from defaults",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md"),
        "builder command from defaults",
    )
    .unwrap();

    // Create existing .claude directory in workspace with custom commands
    fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
    fs::write(
        workspace.join(".claude").join("commands").join("custom.md"),
        "my custom command",
    )
    .unwrap();
    fs::write(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
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
        fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md")).unwrap();
    assert_eq!(custom_content, "my custom command");

    // Verify loom.md was UPDATED with new content (default file)
    // .claude/ always force-merges on reinstall to propagate command updates
    let loom_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
    )
    .unwrap();
    assert_eq!(loom_content, "loom command from defaults");

    // Verify builder.md was added (new file)
    assert!(workspace
        .join(".claude")
        .join("commands")
        .join("loom")
        .join("builder.md")
        .exists());
    let builder_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
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

/// One of the two root instruction files Loom injects a marker section into.
/// The CLAUDE.md and AGENTS.md install paths share the same marker-handling
/// logic, so each test body below runs once per instance (AGENTS.md added by
/// issue #4479, epic #4167; the markers are independent between the two files).
struct InstructionFile {
    /// Root file name and the name of its full-guide template in `.loom/`.
    name: &'static str,
    /// Suffix the AGENTS.md template carries on its title line.
    title_suffix: &'static str,
    start: &'static str,
    end: &'static str,
    pointer: &'static str,
    /// The other file's marker pair, which must never leak into this file.
    other_start: &'static str,
    other_end: &'static str,
}

const CLAUDE_FILE: InstructionFile = InstructionFile {
    name: "CLAUDE.md",
    title_suffix: "",
    start: LOOM_SECTION_START,
    end: LOOM_SECTION_END,
    pointer: LOOM_ROOT_POINTER,
    other_start: AGENTS_SECTION_START,
    other_end: AGENTS_SECTION_END,
};

const AGENTS_FILE: InstructionFile = InstructionFile {
    name: "AGENTS.md",
    title_suffix: " (AGENTS.md)",
    start: AGENTS_SECTION_START,
    end: AGENTS_SECTION_END,
    pointer: AGENTS_ROOT_POINTER,
    other_start: LOOM_SECTION_START,
    other_end: LOOM_SECTION_END,
};

impl InstructionFile {
    /// Full-guide template text for this file with the given body.
    fn template(&self, body: &str) -> String {
        format!("# Loom Orchestration - Repository Guide{}\n\n{body}", self.title_suffix)
    }
}

/// Helper to create a standard test setup with an instruction-file template
/// (`CLAUDE.md` or `AGENTS.md`) in defaults.
fn setup_test_with_template(
    temp_dir: &TempDir,
    file_name: &str,
    template_content: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = temp_dir.path().to_path_buf();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with the instruction-file template
    fs::create_dir_all(defaults.join(".loom")).unwrap();
    fs::write(defaults.join(".loom").join(file_name), template_content).unwrap();

    (workspace, defaults)
}

/// Set up a workspace for `kind` with a template carrying `body`, and
/// pre-create `.loom/` (as `initialize_workspace` normally does).
fn setup_kind(
    temp_dir: &TempDir,
    kind: &InstructionFile,
    body: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let (workspace, defaults) = setup_test_with_template(temp_dir, kind.name, &kind.template(body));
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    (workspace, defaults)
}

fn install(workspace: &std::path::Path, defaults: &std::path::Path, force: bool) -> InitReport {
    let mut report = InitReport::default();
    setup_repository_scaffolding(workspace, defaults, force, &mut report).unwrap();
    report
}

/// Generate a `#[test]` per instruction file from one shared check body.
macro_rules! mirrored_tests {
    ($check:ident, $claude_test:ident, $agents_test:ident) => {
        #[test]
        fn $claude_test() {
            $check(&CLAUDE_FILE);
        }

        #[test]
        fn $agents_test() {
            $check(&AGENTS_FILE);
        }
    };
}

fn check_written_to_loom_dir(kind: &InstructionFile) {
    // Verifies full content goes to .loom/<file> on fresh install
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Full guide content here.");

    let report = install(&workspace, &defaults, false);

    assert!(workspace.join(".loom").join(kind.name).exists());
    let loom_content = fs::read_to_string(workspace.join(".loom").join(kind.name)).unwrap();
    assert!(loom_content.contains("Loom Orchestration - Repository Guide"));
    assert!(loom_content.contains("Full guide content here"));
    assert!(report.added.contains(&format!(".loom/{}", kind.name)));
}
mirrored_tests!(
    check_written_to_loom_dir,
    test_loom_claude_md_written_to_loom_dir,
    test_loom_agents_md_written_to_loom_dir
);

fn check_root_contains_only_pointer(kind: &InstructionFile) {
    // Verifies the root file has the short pointer, not the full guide, on fresh install
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Full guide content here.");

    assert!(!workspace.join(kind.name).exists());

    let report = install(&workspace, &defaults, false);

    assert!(workspace.join(kind.name).exists());
    let root_content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(root_content.contains(kind.start));
    assert!(root_content.contains(kind.end));
    assert!(root_content.contains(kind.pointer));
    // Full guide content must NOT be in the root file
    assert!(!root_content.contains("Full guide content here"));
    assert!(report.added.contains(&kind.name.to_string()));

    // The two files' markers are independent: the root file must not
    // contain the other file's marker pair.
    assert!(!root_content.contains(kind.other_start));
    assert!(!root_content.contains(kind.other_end));
}
mirrored_tests!(
    check_root_contains_only_pointer,
    test_root_claude_md_contains_only_pointer,
    test_root_agents_md_contains_only_pointer
);

fn check_preservation_new_install(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Loom content here.");

    // No existing root file in workspace
    assert!(!workspace.join(kind.name).exists());

    let report = install(&workspace, &defaults, false);

    // Root file created with section markers and short pointer only
    assert!(workspace.join(kind.name).exists());
    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(content.contains(kind.start));
    assert!(content.contains(kind.end));
    assert!(content.contains(kind.pointer));
    // Full content must be absent from root
    assert!(!content.contains("Loom content here"));
    assert!(report.added.contains(&kind.name.to_string()));

    // .loom/<file> created with full content
    assert!(workspace.join(".loom").join(kind.name).exists());
    let loom_content = fs::read_to_string(workspace.join(".loom").join(kind.name)).unwrap();
    assert!(loom_content.contains("Loom content here"));
}
mirrored_tests!(
    check_preservation_new_install,
    test_claude_md_preservation_new_install,
    test_agents_md_preservation_new_install
);

fn check_preservation_existing_project_content(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "New Loom content.");

    // Existing root file with project-specific content (no markers)
    fs::write(
        workspace.join(kind.name),
        r"# My Awesome Project

This project does amazing things with Rust.

## Getting Started

Run `cargo run` to start.",
    )
    .unwrap();

    // Loom pointer should be appended at end
    install(&workspace, &defaults, false);

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(content.contains("My Awesome Project"));
    assert!(content.contains("amazing things with Rust"));
    assert!(content.contains(kind.start));
    assert!(content.contains(kind.end));
    assert!(content.contains(kind.pointer));
    // Full Loom guide must NOT be in root
    assert!(!content.contains("New Loom content"));

    // Project content should come BEFORE Loom section (appended at end)
    let project_pos = content.find("My Awesome Project").unwrap();
    let loom_pos = content.find(kind.start).unwrap();
    assert!(project_pos < loom_pos);

    // No duplicate content
    assert_eq!(content.matches("My Awesome Project").count(), 1);
}
mirrored_tests!(
    check_preservation_existing_project_content,
    test_claude_md_preservation_existing_project_content,
    test_agents_md_preservation_existing_project_content
);

fn check_append_when_no_markers(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Loom content here.");

    // Existing root file WITHOUT markers (e.g., from previous install or manual creation)
    fs::write(
        workspace.join(kind.name),
        r"# Lean Genius Project

Formal mathematics in Lean 4.

## Docker Build Safety

WARNING: Never run `lake build` inside Docker - causes memory corruption.

## Custom Agents

- Erdos: Mathematical proof orchestrator
- Aristotle: Automated theorem prover",
    )
    .unwrap();

    install(&workspace, &defaults, true);

    // Existing content was preserved at top
    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(content.contains("Lean Genius Project"));
    assert!(content.contains("Docker Build Safety"));
    assert!(content.contains("Custom Agents"));

    // Loom pointer was appended at end with markers
    assert!(content.contains(kind.start));
    assert!(content.contains(kind.end));
    assert!(content.contains(kind.pointer));
    // Full guide must NOT be in root
    assert!(!content.contains("Loom content here"));

    // Order: project content comes BEFORE Loom section
    let project_pos = content.find("Lean Genius Project").unwrap();
    let loom_pos = content.find(kind.start).unwrap();
    assert!(project_pos < loom_pos);

    // No duplicate content or mangling
    assert_eq!(content.matches("Lean Genius Project").count(), 1);
}
mirrored_tests!(
    check_append_when_no_markers,
    test_claude_md_append_when_no_markers,
    test_agents_md_append_when_no_markers
);

fn check_preservation_update_loom_section_only(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "UPDATED Loom content v2.0.");

    // Existing root file with markers already present (simulating a prior install)
    let existing = format!(
        "# My Project\n\nProject docs here.\n\n{}\nOld Loom content v1.0.\n{}",
        kind.start, kind.end
    );
    fs::write(workspace.join(kind.name), existing).unwrap();

    install(&workspace, &defaults, true);

    // Project content preserved, Loom section replaced with short pointer
    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(content.contains("My Project"));
    assert!(content.contains("Project docs here"));
    // Old section content must be gone from root
    assert!(!content.contains("Old Loom content v1.0"));
    // Updated full guide must also NOT be in root
    assert!(!content.contains("UPDATED Loom content v2.0"));
    assert!(content.contains(kind.pointer));

    // Should only have ONE set of markers
    assert_eq!(content.matches(kind.start).count(), 1, "Should have exactly one start marker");
    assert_eq!(content.matches(kind.end).count(), 1, "Should have exactly one end marker");

    // Updated full guide content must be in .loom/<file>
    let loom_content = fs::read_to_string(workspace.join(".loom").join(kind.name)).unwrap();
    assert!(loom_content.contains("UPDATED Loom content v2.0"));
}
mirrored_tests!(
    check_preservation_update_loom_section_only,
    test_claude_md_preservation_update_loom_section_only,
    test_agents_md_preservation_update_loom_section_only
);

fn check_loom_file_updated_on_reinstall(kind: &InstructionFile) {
    // Verifies .loom/<file> is overwritten on reinstall with new template content
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Updated content v2.");

    // Pre-existing .loom/<file> from previous install
    fs::write(workspace.join(".loom").join(kind.name), kind.template("Old content v1.")).unwrap();

    let report = install(&workspace, &defaults, false);

    let loom_content = fs::read_to_string(workspace.join(".loom").join(kind.name)).unwrap();
    assert!(loom_content.contains("Updated content v2"));
    assert!(!loom_content.contains("Old content v1"));
    assert!(report.updated.contains(&format!(".loom/{}", kind.name)));
}
mirrored_tests!(
    check_loom_file_updated_on_reinstall,
    test_loom_claude_md_updated_on_reinstall,
    test_loom_agents_md_updated_on_reinstall
);

fn check_marker_replace_inserts_blank_line_separator(kind: &InstructionFile) {
    // Regression test for #5384. When the root file already carries
    // well-formed Loom markers (the reinstall/upgrade shape), the
    // marker-replace branch previously concatenated `before.trim_end()`
    // directly onto the wrapped pointer with NO separator, gluing the
    // user's last line onto the START marker into one Markdown run-on
    // line. Seed `before` with NO trailing blank line (ends immediately
    // at the marker) to reproduce that exact shape.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Full guide content.");

    let existing = format!(
        "Migrate them here rather than rewriting them — see issue #2.{}\nOld pointer text.\n{}",
        kind.start, kind.end
    );
    fs::write(workspace.join(kind.name), existing).unwrap();

    install(&workspace, &defaults, true);

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    let marker_idx = content
        .find(kind.start)
        .expect("START marker must be present");
    assert!(
        content[..marker_idx].ends_with("\n\n"),
        "expected a blank line (two newlines) immediately before the START marker, got: {:?}",
        &content[marker_idx.saturating_sub(10)..marker_idx]
    );
    assert!(content.contains("Migrate them here"));
}
mirrored_tests!(
    check_marker_replace_inserts_blank_line_separator,
    test_claude_md_marker_replace_inserts_blank_line_separator,
    test_agents_md_marker_replace_inserts_blank_line_separator
);

fn check_marker_replace_reinstall_does_not_accumulate_blank_lines(kind: &InstructionFile) {
    // Regression test for #5384's idempotency requirement: running
    // `setup_repository_scaffolding` repeatedly over an already-correctly-
    // separated marker block must not grow extra blank lines each time.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Full guide content.");

    let existing = format!(
        "# My Project\n\nHand-written docs.{}\nOld pointer text.\n{}",
        kind.start, kind.end
    );
    fs::write(workspace.join(kind.name), existing).unwrap();

    // Run scaffolding four times in a row, simulating repeated reinstalls.
    for _ in 0..4 {
        install(&workspace, &defaults, true);
    }

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    let marker_idx = content
        .find(kind.start)
        .expect("START marker must be present");
    let prefix = &content[..marker_idx];
    assert!(
        prefix.ends_with("\n\n") && !prefix.ends_with("\n\n\n"),
        "expected exactly one blank line before START marker after repeat reinstalls, got: {:?}",
        &prefix[prefix.len().saturating_sub(10)..]
    );
    assert_eq!(
        content.matches(kind.start).count(),
        1,
        "Should have exactly one start marker after repeat reinstalls"
    );
}
mirrored_tests!(
    check_marker_replace_reinstall_does_not_accumulate_blank_lines,
    test_claude_md_marker_replace_reinstall_does_not_accumulate_blank_lines,
    test_agents_md_marker_replace_reinstall_does_not_accumulate_blank_lines
);

// =========================================================================
// Trailing-newline normalization tests (issue #6331). Both root CLAUDE.md
// and root AGENTS.md must always end with exactly one `\n` after install,
// regardless of assembly branch or whether the pre-existing source content
// itself ended with a newline. Covers: fresh file, no-markers append
// (with and without a trailing newline on the source), existing-markers
// replace, and malformed-markers append — mirrored for both files.
// =========================================================================

fn check_fresh_install_ends_with_newline(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Full guide content here.");

    // No existing root file - exercises the fresh-file branch.
    assert!(!workspace.join(kind.name).exists());

    install(&workspace, &defaults, false);

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(
        content.ends_with('\n'),
        "fresh {} must end with a newline: {content:?}",
        kind.name
    );
}
mirrored_tests!(
    check_fresh_install_ends_with_newline,
    test_claude_md_fresh_install_ends_with_newline,
    test_agents_md_fresh_install_ends_with_newline
);

fn check_no_markers_append_ends_with_newline(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "New Loom content.");

    // Existing markerless root file that already ends with a newline.
    fs::write(workspace.join(kind.name), "# My Project\n\nHand-authored project docs.\n").unwrap();

    install(&workspace, &defaults, false);

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(
        content.ends_with('\n'),
        "no-markers append must end with a newline: {content:?}"
    );
    // Sanity: exactly one trailing newline, not an accumulating run.
    assert!(!content.ends_with("\n\n"), "must not accumulate blank lines: {content:?}");
}
mirrored_tests!(
    check_no_markers_append_ends_with_newline,
    test_claude_md_no_markers_append_ends_with_newline,
    test_agents_md_no_markers_append_ends_with_newline
);

fn check_no_markers_append_self_heals_missing_source_newline(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "New Loom content.");

    // Existing markerless root file with NO trailing newline at all.
    fs::write(
        workspace.join(kind.name),
        "# My Project\n\nHand-authored project docs, no trailing newline.",
    )
    .unwrap();

    install(&workspace, &defaults, false);

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(
        content.ends_with('\n'),
        "must self-heal a source file with no trailing newline: {content:?}"
    );
}
mirrored_tests!(
    check_no_markers_append_self_heals_missing_source_newline,
    test_claude_md_no_markers_append_self_heals_missing_source_newline,
    test_agents_md_no_markers_append_self_heals_missing_source_newline
);

fn check_existing_markers_replace_ends_with_newline(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "UPDATED Loom content v2.0.");

    // Existing root file with markers already present, no trailing newline
    // (simulating a prior install this fix has not yet touched).
    let existing = format!(
        "# My Project\n\nProject docs here.\n\n{}\nOld pointer text.\n{}",
        kind.start, kind.end
    );
    fs::write(workspace.join(kind.name), existing).unwrap();

    install(&workspace, &defaults, true);

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(
        content.ends_with('\n'),
        "existing-markers replace must self-heal to a trailing newline: {content:?}"
    );
}
mirrored_tests!(
    check_existing_markers_replace_ends_with_newline,
    test_claude_md_existing_markers_replace_ends_with_newline,
    test_agents_md_existing_markers_replace_ends_with_newline
);

fn check_malformed_markers_append_ends_with_newline(kind: &InstructionFile) {
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_kind(&temp_dir, kind, "Full guide content (new).");

    // Only the START marker is present (no END) - malformed-markers branch.
    let malformed = format!("{}\nno end marker here", kind.start);
    fs::write(workspace.join(kind.name), &malformed).unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), "install must not fail on malformed markers: {result:?}");

    let content = fs::read_to_string(workspace.join(kind.name)).unwrap();
    assert!(
        content.ends_with('\n'),
        "malformed-markers append must end with a newline: {content:?}"
    );
}
mirrored_tests!(
    check_malformed_markers_append_ends_with_newline,
    test_claude_md_malformed_markers_append_ends_with_newline,
    test_agents_md_malformed_markers_append_ends_with_newline
);

// ---------- #4888 AGENTS.md legacy-placeholder migration tests ----------

#[test]
fn test_setup_scaffolding_discards_markerless_legacy_root_agents_md() {
    // Regression test for #4888 defect 1. A broken/interrupted prior
    // install (or an old pre-marker layout) can leave a markerless root
    // AGENTS.md carrying leaked, unsubstituted `{{LOOM_VERSION}}` text.
    // Before the fix, the "no markers" branch preserved this verbatim,
    // which reintroduced the placeholders and tripped
    // `assert_no_placeholders`, aborting the whole install.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "AGENTS.md",
        "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    // Markerless legacy content with leaked template placeholders.
    fs::write(
        workspace.join("AGENTS.md"),
        "# Loom Orchestration - Repository Guide\n\n\
             **Loom Version**: {{LOOM_VERSION}}\n\
             **Installation Date**: {{INSTALL_DATE}}\n\n\
             Generated by Loom Installation Process\n",
    )
    .unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), "install must not fail on legacy AGENTS.md: {result:?}");

    let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(content.contains(AGENTS_SECTION_START));
    assert!(content.contains(AGENTS_SECTION_END));
    assert!(content.contains(AGENTS_ROOT_POINTER));
    assert!(!content.contains("{{LOOM_VERSION}}"), "leaked placeholder: {content}");
    assert!(!content.contains("{{INSTALL_DATE}}"));
    assert!(!content.contains("Generated by Loom Installation Process"));
}

#[test]
fn test_setup_scaffolding_discards_hybrid_legacy_root_agents_md() {
    // Regression test for #4888 defect 1, hybrid variant (mirrors
    // CLAUDE.md's #3476 hybrid test): a markered root AGENTS.md whose
    // slice OUTSIDE the marker block is itself leftover legacy content
    // with unsubstituted placeholders. Before the fix the marker-replace
    // branch preserved `before`/`after` verbatim regardless of content,
    // leaking the placeholders through and tripping the guard.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "AGENTS.md",
        "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    let legacy_fragment = "# Loom Orchestration - Repository Guide\n\n\
             **Loom Version**: {{LOOM_VERSION}}\n\
             **Installation Date**: {{INSTALL_DATE}}\n\n\
             Generated by Loom Installation Process\n";
    let hybrid = format!("{}\n{}\n", legacy_fragment, wrap_agents_content("Old pointer text"));
    fs::write(workspace.join("AGENTS.md"), &hybrid).unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), "install must not fail on hybrid legacy AGENTS.md: {result:?}");

    let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    // Trailing newline is normalized on write (issue #6331), so compare
    // against the wrapped pointer plus that normalization rather than the
    // raw (newline-less) `wrap_agents_content` output.
    assert_eq!(
        content,
        format!("{}\n", wrap_agents_content(AGENTS_ROOT_POINTER)),
        "hybrid legacy AGENTS.md should be fully replaced with the wrapped pointer"
    );
    assert!(!content.contains("{{LOOM_VERSION}}"));
    assert!(!content.contains("{{INSTALL_DATE}}"));
    assert!(!content.contains("Generated by Loom Installation Process"));
    assert!(!content.contains("Old pointer text"));
}

#[test]
fn test_setup_scaffolding_discards_malformed_marker_legacy_root_agents_md() {
    // Regression test for #4888 defect 1, malformed-marker variant: only
    // the START marker is present (no END), which used to fall into the
    // "append pointer at end" branch and preserve the entire file
    // (including leaked placeholders) verbatim.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "AGENTS.md",
        "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    let malformed = format!(
        "{AGENTS_SECTION_START}\n**Loom Version**: {{{{LOOM_VERSION}}}}\nno end marker here\n"
    );
    fs::write(workspace.join("AGENTS.md"), &malformed).unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(
        result.is_ok(),
        "install must not fail on malformed-marker AGENTS.md: {result:?}"
    );

    let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(content.contains(AGENTS_SECTION_START));
    assert!(content.contains(AGENTS_SECTION_END));
    assert!(content.contains(AGENTS_ROOT_POINTER));
    assert!(!content.contains("{{LOOM_VERSION}}"), "leaked placeholder: {content}");
}

#[test]
fn test_setup_scaffolding_preserves_markerless_user_root_agents_md() {
    // Negative control: markerless content with NO legacy signature must
    // still be preserved and appended-to, not discarded. Guards against
    // the new legacy check being overly aggressive.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "AGENTS.md",
        "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    fs::write(
        workspace.join("AGENTS.md"),
        "# My Project\n\nHand-written Codex instructions, no Loom signatures here.",
    )
    .unwrap();

    let mut report = InitReport::default();
    setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

    let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(content.contains("Hand-written Codex instructions"));
    assert!(content.contains(AGENTS_SECTION_START));
    assert!(content.contains(AGENTS_ROOT_POINTER));
}

#[test]
fn test_codex_directory_copy_is_silent_noop_when_absent() {
    // `defaults/.codex/` does not currently ship in this repo. Verify that
    // running scaffolding setup with no `defaults/.codex/` present does not
    // error, does not create `<workspace>/.codex/`, and does not add any
    // report entries referencing `.codex`.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(&defaults).unwrap();
    // Deliberately do NOT create defaults/.codex/.
    assert!(!defaults.join(".codex").exists());

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), ".codex/ absence must not error: {result:?}");

    assert!(
        !workspace.join(".codex").exists(),
        ".codex/ must not be created in the workspace when defaults/.codex/ is absent"
    );
    assert!(
        !report
            .added
            .iter()
            .chain(report.updated.iter())
            .chain(report.preserved.iter())
            .any(|p| p.contains(".codex")),
        "no report entries should reference .codex when the source directory is absent"
    );
}

#[test]
fn test_claude_commands_always_updated_on_reinstall() {
    // .claude/ commands should always be force-merged on reinstall (without --force flag)
    // This ensures command updates propagate while custom commands are preserved.
    // Issue #3310: also covers `.claude/agents/` (subagent definitions),
    // which live alongside `.claude/commands/` and must propagate the same way.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with .claude commands AND agents
    fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
    fs::create_dir_all(defaults.join(".claude").join("agents")).unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
        "loom command v2 with bug fix",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md"),
        "builder command v2",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("agents")
            .join("loom-builder.md"),
        "loom-builder subagent v2",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("agents")
            .join("loom-judge.md"),
        "loom-judge subagent v1",
    )
    .unwrap();

    // Create existing .claude directory in workspace (simulates previous install)
    fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
    fs::create_dir_all(workspace.join(".claude").join("agents")).unwrap();
    fs::write(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
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
    fs::write(
        workspace
            .join(".claude")
            .join("agents")
            .join("loom-builder.md"),
        "loom-builder subagent v1 with bug",
    )
    .unwrap();
    fs::write(
        workspace
            .join(".claude")
            .join("agents")
            .join("my-custom-agent.md"),
        "project-specific custom subagent",
    )
    .unwrap();

    // Run setup WITHOUT force flag (simulates normal reinstall)
    let mut report = InitReport::default();
    setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

    // Verify: loom.md was UPDATED (default command updated with bug fix)
    let loom_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("loom.md"),
    )
    .unwrap();
    assert_eq!(loom_content, "loom command v2 with bug fix");

    // Verify: builder.md was ADDED (new default command)
    let builder_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("commands")
            .join("loom")
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
        .contains(&".claude/commands/loom/loom.md".to_string()));
    assert!(report
        .added
        .contains(&".claude/commands/loom/builder.md".to_string()));
    assert!(report
        .preserved
        .contains(&".claude/commands/my-custom.md".to_string()));

    // Issue #3310: verify .claude/agents/ propagates identically.
    // Default subagent updated:
    let agent_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("agents")
            .join("loom-builder.md"),
    )
    .unwrap();
    assert_eq!(agent_content, "loom-builder subagent v2");
    // New default subagent added:
    let new_agent_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("agents")
            .join("loom-judge.md"),
    )
    .unwrap();
    assert_eq!(new_agent_content, "loom-judge subagent v1");
    // Project-specific custom subagent preserved:
    let custom_agent_content = fs::read_to_string(
        workspace
            .join(".claude")
            .join("agents")
            .join("my-custom-agent.md"),
    )
    .unwrap();
    assert_eq!(custom_agent_content, "project-specific custom subagent");

    assert!(report
        .updated
        .contains(&".claude/agents/loom-builder.md".to_string()));
    assert!(report
        .added
        .contains(&".claude/agents/loom-judge.md".to_string()));
    assert!(report
        .preserved
        .contains(&".claude/agents/my-custom-agent.md".to_string()));
}

#[test]
fn test_fresh_install_copies_claude_agents() {
    // Issue #3310: a fresh install (no existing .claude/) must copy the
    // full `.claude/agents/` tree from defaults so native subagent
    // dispatch (subagent_type="loom-builder", etc.) works out of the
    // box. Previously the .claude/ directory copy happened only via
    // copy_dir_with_report — this test pins the behavior so the
    // installer cannot silently regress agents/ in the future.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with .claude/agents/ (and a commands/ stub so the
    // .claude/ src directory is non-empty, mirroring real defaults).
    fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
    fs::create_dir_all(defaults.join(".claude").join("agents")).unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md"),
        "builder command",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("agents")
            .join("loom-builder.md"),
        "loom-builder subagent body",
    )
    .unwrap();
    fs::write(
        defaults
            .join(".claude")
            .join("agents")
            .join("loom-judge.md"),
        "loom-judge subagent body",
    )
    .unwrap();

    // Workspace starts with NO .claude/ — pure fresh install
    assert!(!workspace.join(".claude").exists());

    let mut report = InitReport::default();
    setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

    // The fresh-install path must produce the full agents tree.
    let installed_builder = workspace
        .join(".claude")
        .join("agents")
        .join("loom-builder.md");
    assert!(
        installed_builder.exists(),
        "fresh install must copy .claude/agents/loom-builder.md (see #3310)"
    );
    assert_eq!(fs::read_to_string(&installed_builder).unwrap(), "loom-builder subagent body");
    assert!(workspace
        .join(".claude")
        .join("agents")
        .join("loom-judge.md")
        .exists());

    // Report should track every agent file as "added".
    assert!(report
        .added
        .contains(&".claude/agents/loom-builder.md".to_string()));
    assert!(report
        .added
        .contains(&".claude/agents/loom-judge.md".to_string()));
}

// =========================================================================
// settings.json merge tests
// =========================================================================

#[test]
fn test_merge_settings_fresh_install_no_existing() {
    // When no existing settings.json, Loom defaults are used as-is
    let loom_defaults: serde_json::Value = serde_json::from_str(
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)"]
            }
        }"#,
    )
    .unwrap();

    // Empty existing
    let existing: serde_json::Value = serde_json::from_str("{}").unwrap();
    let merged = merge_settings_json(&existing, &loom_defaults);

    // Should have Loom's hooks
    let hooks = merged.get("hooks").unwrap();
    let pre_tool = hooks.get("PreToolUse").unwrap().as_array().unwrap();
    assert_eq!(pre_tool.len(), 1);
    assert_eq!(pre_tool[0]["matcher"], "Bash");

    // Should have Loom's permissions
    let perms = merged
        .get("permissions")
        .unwrap()
        .get("allow")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(perms.len(), 2);
}

#[test]
fn test_merge_settings_preserves_project_hooks() {
    let existing: serde_json::Value = serde_json::from_str(
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Edit",
                    "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk-files.sh"}]
                }],
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [{"type": "command", "command": "skill-router.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "CustomPermission"]
            }
        }"#,
    )
    .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)"]
            }
        }"#,
    )
    .unwrap();

    let merged = merge_settings_json(&existing, &loom_defaults);

    // PreToolUse should have both Edit (project) and Bash (Loom) matchers
    let pre_tool = merged["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_tool.len(), 2, "Should have both Edit and Bash matchers");

    // Edit matcher should be preserved
    let edit_matcher = pre_tool.iter().find(|m| m["matcher"] == "Edit").unwrap();
    assert_eq!(edit_matcher["hooks"][0]["command"], ".claude/hooks/guard-pdk-files.sh");

    // Bash matcher should be added from Loom
    let bash_matcher = pre_tool.iter().find(|m| m["matcher"] == "Bash").unwrap();
    assert_eq!(bash_matcher["hooks"][0]["command"], ".loom/hooks/guard-destructive.sh");

    // UserPromptSubmit (project-only) should be preserved
    let user_prompt = merged["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(user_prompt.len(), 1);
    assert_eq!(user_prompt[0]["hooks"][0]["command"], "skill-router.sh");

    // Permissions should be unioned (3 unique: gh, git, CustomPermission)
    let perms = merged["permissions"]["allow"].as_array().unwrap();
    assert_eq!(perms.len(), 3, "Should have 3 unique permissions");
    let perm_strs: Vec<&str> = perms.iter().map(|p| p.as_str().unwrap()).collect();
    assert!(perm_strs.contains(&"Bash(gh:*)"));
    assert!(perm_strs.contains(&"Bash(git:*)"));
    assert!(perm_strs.contains(&"CustomPermission"));
}

#[test]
fn test_merge_settings_deduplicates_hooks() {
    // When project already has the new-prefix Loom hook, don't add it again
    let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

    let merged = merge_settings_json(&existing, &loom_defaults);

    // Should not duplicate the Loom hook
    let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
    let hooks_arr = bash_hooks.as_array().unwrap();
    assert_eq!(hooks_arr.len(), 2, "Should not duplicate existing Loom hook");

    // Both hooks should still be present
    let commands: Vec<&str> = hooks_arr
        .iter()
        .map(|h| h["command"].as_str().unwrap())
        .collect();
    assert!(commands.contains(&"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"));
    assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));
}

#[test]
fn test_merge_settings_deduplicates_hooks_with_quoted_paths() {
    // Issue #4200: a prior installer generation wrote the Loom hook
    // command wrapped in double quotes (to survive a project path
    // containing spaces). Reinstalling with the current, unquoted
    // template must recognize this as the SAME hook and not append a
    // second, functionally identical entry.
    let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

    let merged = merge_settings_json(&existing, &loom_defaults);

    let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
    let hooks_arr = bash_hooks.as_array().unwrap();

    // Exactly 2 entries: the original quoted Loom hook (preserved as-is,
    // not rewritten) + the custom project hook. No unquoted duplicate.
    assert_eq!(
        hooks_arr.len(),
        2,
        "Should not append an unquoted duplicate of an existing quoted Loom hook: {hooks_arr:?}"
    );

    let commands: Vec<&str> = hooks_arr
        .iter()
        .map(|h| h["command"].as_str().unwrap())
        .collect();
    assert!(
            commands.contains(&"\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""),
            "Original quoted entry should be preserved unchanged (comparison-only normalization), got: {commands:?}"
        );
    assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));
}

#[test]
fn test_merge_settings_migrates_legacy_hooks() {
    // Pre-3265 installs have bare-relative `.loom/hooks/...` entries.
    // On re-install, the merge must strip the legacy entry and add the new
    // `${CLAUDE_PROJECT_DIR}/.loom/hooks/...` entry so the result has no
    // duplicate invocations.
    let existing: serde_json::Value = serde_json::from_str(
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": ".loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
    )
    .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

    let merged = merge_settings_json(&existing, &loom_defaults);

    let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
    let hooks_arr = bash_hooks.as_array().unwrap();

    let commands: Vec<&str> = hooks_arr
        .iter()
        .map(|h| h["command"].as_str().unwrap())
        .collect();

    // Legacy bare-relative entry must be stripped
    assert!(
        !commands.contains(&".loom/hooks/guard-destructive.sh"),
        "Legacy bare-relative hook should be removed during merge"
    );
    // New prefix entry must be present
    assert!(
        commands.contains(&"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"),
        "New ${{CLAUDE_PROJECT_DIR}}-prefixed hook must be added"
    );
    // Custom project hook must be preserved
    assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));

    // Exactly 2 entries: new Loom hook + custom project hook (no duplicate)
    assert_eq!(
        hooks_arr.len(),
        2,
        "Should have exactly 2 hooks: new Loom hook + custom project hook"
    );
}

#[test]
fn test_merge_settings_preserves_other_keys() {
    // Keys like enabledPlugins, MCP config, etc. should be preserved
    let existing: serde_json::Value = serde_json::from_str(
        r#"{
            "enabledPlugins": {"some-plugin": true},
            "model": "opus",
            "permissions": {
                "allow": ["CustomPermission"]
            }
        }"#,
    )
    .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)"]
            }
        }"#,
    )
    .unwrap();

    let merged = merge_settings_json(&existing, &loom_defaults);

    // enabledPlugins and model should be preserved
    assert_eq!(merged["enabledPlugins"]["some-plugin"], true);
    assert_eq!(merged["model"], "opus");

    // Hooks should be added
    assert!(merged.get("hooks").is_some());

    // Permissions should be merged
    let perms = merged["permissions"]["allow"].as_array().unwrap();
    assert_eq!(perms.len(), 2);
}

#[test]
fn test_merge_settings_in_scaffolding_reinstall() {
    // Integration test: verify settings.json is merged during reinstall
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with .claude/commands and settings.json
    fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
    fs::write(defaults.join(".claude").join("commands").join("loom.md"), "loom command").unwrap();
    fs::write(
            defaults.join(".claude").join("settings.json"),
            r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                    }]
                },
                "permissions": {
                    "allow": ["Bash(gh:*)", "Bash(git:*)"]
                }
            }"#,
        ).unwrap();

    // Create existing .claude directory with project settings
    fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
    fs::write(
        workspace.join(".claude").join("settings.json"),
        r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Edit",
                        "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk.sh"}]
                    }],
                    "UserPromptSubmit": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "skill-router.sh"}]
                    }]
                },
                "permissions": {
                    "allow": ["Bash(gh:*)", "CustomPermission"]
                },
                "enabledPlugins": {"my-plugin": true}
            }"#,
    )
    .unwrap();

    // Run setup (reinstall)
    let mut report = InitReport::default();
    setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

    // Read the resulting settings.json
    let result_content =
        fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
    let result: serde_json::Value = serde_json::from_str(&result_content).unwrap();

    // PreToolUse should have both matchers (Edit from project, Bash from Loom)
    let pre_tool = result["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_tool.len(), 2, "Should have Edit and Bash matchers");

    // Project's Edit matcher should be preserved
    let has_edit = pre_tool.iter().any(|m| m["matcher"] == "Edit");
    assert!(has_edit, "Project's Edit matcher should be preserved");

    // Loom's Bash matcher should be added
    let has_bash = pre_tool.iter().any(|m| m["matcher"] == "Bash");
    assert!(has_bash, "Loom's Bash matcher should be added");

    // Project's UserPromptSubmit should be preserved
    assert!(
        result["hooks"].get("UserPromptSubmit").is_some(),
        "Project's UserPromptSubmit hooks should be preserved"
    );

    // Permissions should be unioned
    let perms = result["permissions"]["allow"].as_array().unwrap();
    let perm_strs: Vec<&str> = perms.iter().map(|p| p.as_str().unwrap()).collect();
    assert!(perm_strs.contains(&"Bash(gh:*)"));
    assert!(perm_strs.contains(&"Bash(git:*)"));
    assert!(perm_strs.contains(&"CustomPermission"));

    // enabledPlugins should be preserved
    assert_eq!(result["enabledPlugins"]["my-plugin"], true);
}

/// Issue #5279 edge case: a foreign hook under a hook type/matcher pair
/// Loom's own defaults ALSO define (both register `PreToolUse`/`Bash`).
/// Confirms dedup is by normalized *command*, not by hook type or matcher
/// alone -- a foreign command sharing Loom's matcher must survive
/// alongside Loom's own command, not be treated as a collision and
/// dropped/replaced.
#[test]
fn test_merge_settings_dedup_preserves_foreign_command_in_shared_matcher() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
    fs::write(
            defaults.join(".claude").join("settings.json"),
            r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                    }]
                }
            }"#,
        )
        .unwrap();

    fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
    fs::write(
        workspace.join(".claude").join("settings.json"),
        r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "repo-skills-bash-guard.sh"}]
                    }]
                }
            }"#,
    )
    .unwrap();

    let mut report = InitReport::default();
    setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

    let result_content =
        fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
    let result: serde_json::Value = serde_json::from_str(&result_content).unwrap();

    let pre_tool = result["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_tool.len(), 1, "Bash matcher should not be duplicated");

    let bash_matcher = &pre_tool[0];
    let commands: Vec<&str> = bash_matcher["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["command"].as_str().unwrap())
        .collect();

    assert!(
        commands.contains(&"repo-skills-bash-guard.sh"),
        "Foreign command sharing Loom's hook type+matcher must survive: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.contains("guard-destructive.sh")),
        "Loom's own command must also be present: {commands:?}"
    );
}

/// Issue #5279 ("Suspected Cause" #2): a pre-existing `.claude/settings.json`
/// that fails to parse as JSON (e.g. a sibling tool wrote non-strict JSON
/// with a trailing comma) must not silently and invisibly discard the
/// existing file's content -- `read_existing_settings` skips merging in
/// this case (documented behavior), and this test locks in that the skip
/// happens cleanly (no panic, Loom's own settings.json is written) so a
/// regression can't turn this into a crash. The accompanying warning text
/// (see `read_existing_settings`'s doc comment) is intentionally not
/// assertable here -- it goes to stderr, matching this file's other
/// eprintln!-based warnings (e.g. the settings-write-failure warning in
/// `setup_repository_scaffolding`), none of which are stderr-asserted.
#[test]
fn test_merge_settings_skips_merge_on_invalid_json_existing_settings() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
    fs::write(
        defaults.join(".claude").join("settings.json"),
        r#"{
                "permissions": {
                    "allow": ["Bash(gh:*)"]
                }
            }"#,
    )
    .unwrap();

    fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
    // Deliberately invalid JSON: a trailing comma after the last array
    // element, as a non-strict-JSON-tolerant tool might emit.
    fs::write(
        workspace.join(".claude").join("settings.json"),
        r#"{
                "permissions": {
                    "allow": ["Bash(repo-skills-tool:*)",]
                },
                "enabledPlugins": {"repo-skills": true}
            }"#,
    )
    .unwrap();

    let mut report = InitReport::default();
    // Must not panic, and must complete the install.
    setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

    let result_content =
        fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
    let result: serde_json::Value =
        serde_json::from_str(&result_content).expect("Loom's own settings.json must be valid JSON");

    // The merge was skipped (documented current behavior for unparseable
    // existing content) -- Loom's own settings.json is what's on disk, and
    // the foreign content is NOT present, since there was nothing valid to
    // merge it from.
    assert_eq!(
        result["permissions"]["allow"][0], "Bash(gh:*)",
        "Loom's own settings.json should be in place"
    );
    assert!(
        result.get("enabledPlugins").is_none(),
        "Foreign content from the unparseable file cannot survive a skipped merge"
    );
}

// ---------- #3325 legacy-layout migration tests ----------

/// Snippet of the legacy (pre-#3000) root CLAUDE.md content, including
/// unsubstituted template placeholders. Real-world example: strata-fdtd at
/// commit `e1776eed` had this exact shape.
const LEGACY_ROOT_CLAUDE_MD: &str = "\
# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Some stale guide content here that nobody should preserve on upgrade.

---

**Generated by Loom Installation Process**
";

#[test]
fn test_is_legacy_loom_managed_root_detects_old_layout() {
    // Old-layout content with the title header is legacy.
    assert!(is_legacy_loom_managed_root(LEGACY_ROOT_CLAUDE_MD));

    // Bare `{{LOOM_VERSION}}` is also a signature on its own.
    assert!(is_legacy_loom_managed_root("Loom Version: {{LOOM_VERSION}}"));

    // "Generated by Loom Installation Process" footer alone is enough.
    assert!(is_legacy_loom_managed_root(
        "Some content.\n\n**Generated by Loom Installation Process**"
    ));
}

#[test]
fn test_is_legacy_loom_managed_root_rejects_user_content() {
    // Pure user content with no Loom signatures.
    assert!(!is_legacy_loom_managed_root(
        "# My Project\n\nThis is hand-written documentation."
    ));

    // Empty file.
    assert!(!is_legacy_loom_managed_root(""));

    // Mentioning "loom" in passing isn't enough — we require a specific
    // installer-generated phrase.
    assert!(!is_legacy_loom_managed_root(
        "# My Project\n\nWe use loom for some stuff but wrote this ourselves."
    ));
}

#[test]
fn test_is_legacy_loom_managed_root_skips_marker_block() {
    // Modern marker block must short-circuit to "not legacy" regardless of
    // signature phrases inside the block — that branch is handled
    // separately by the section-replace logic upstream.
    let modern = format!(
            "{LOOM_SECTION_START}\nThis repository uses [Loom](...). **Loom Version**: 0.8.0\n{LOOM_SECTION_END}"
        );
    assert!(!is_legacy_loom_managed_root(&modern));
}

#[test]
fn test_setup_scaffolding_upgrades_legacy_root_claude_md() {
    // Regression test for #3325. Pre-create a workspace root CLAUDE.md
    // with the legacy full-guide layout (including unsubstituted
    // placeholders), run scaffolding, assert the result is the modern
    // marker block — no leftover placeholders, no leftover legacy content.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "CLAUDE.md",
        "# Loom Orchestration - Repository Guide\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    // Pre-existing legacy root CLAUDE.md (pre-#3000 layout).
    fs::write(workspace.join("CLAUDE.md"), LEGACY_ROOT_CLAUDE_MD).unwrap();

    let mut report = InitReport::default();
    setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

    let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

    // Result must be the bare marker block — legacy content is gone.
    assert!(content.contains(LOOM_SECTION_START));
    assert!(content.contains(LOOM_SECTION_END));
    assert!(content.contains(LOOM_ROOT_POINTER));
    // No leaked placeholders.
    assert!(
        !content.contains("{{LOOM_VERSION}}"),
        "leaked {{{{LOOM_VERSION}}}} placeholder: {content}"
    );
    assert!(!content.contains("{{LOOM_COMMIT}}"));
    assert!(!content.contains("{{INSTALL_DATE}}"));
    // No leftover legacy content from the title-header block.
    assert!(
        !content.contains("# Loom Orchestration - Repository Guide"),
        "legacy header should be replaced, got: {content}"
    );
    assert!(!content.contains("Some stale guide content"));
    assert!(!content.contains("Generated by Loom Installation Process"));
}

#[test]
fn test_setup_scaffolding_upgrades_hybrid_legacy_root_claude_md() {
    // Regression test for #3476 Bug 1. The v0.7.1 installer wrote the
    // full legacy guide (with unsubstituted placeholders) to root
    // CLAUDE.md AND appended the modern marker block — a hybrid file.
    // The marker-replacement branch used to preserve the legacy "before"
    // portion verbatim, so assert_no_placeholders refused the write and
    // the upgrade aborted. The fix detects the legacy slice and replaces
    // the entire file with the wrapped pointer.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "CLAUDE.md",
        "# Loom Orchestration - Repository Guide\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    // Hybrid shape: legacy full guide followed by a modern marker block.
    let hybrid = format!("{}\n{}\n", LEGACY_ROOT_CLAUDE_MD, wrap_loom_content("Old pointer text"));
    fs::write(workspace.join("CLAUDE.md"), &hybrid).unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), "upgrade of hybrid legacy CLAUDE.md failed: {result:?}");

    let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

    // Result must be ONLY the wrapped pointer block. Trailing newline is
    // normalized on write (issue #6331), so compare against the wrapped
    // pointer plus that normalization rather than the raw output of
    // `wrap_loom_content`, which itself has no trailing newline.
    assert_eq!(
        content,
        format!("{}\n", wrap_loom_content(LOOM_ROOT_POINTER)),
        "hybrid legacy file should be fully replaced with the wrapped pointer"
    );
    // Exactly one marker block — the legacy portion is gone, not duplicated.
    assert_eq!(content.matches(LOOM_SECTION_START).count(), 1);
    assert_eq!(content.matches(LOOM_SECTION_END).count(), 1);
    // No leaked placeholders or legacy content.
    assert!(!content.contains("{{LOOM_VERSION}}"));
    assert!(!content.contains("{{LOOM_COMMIT}}"));
    assert!(!content.contains("{{INSTALL_DATE}}"));
    assert!(!content.contains("# Loom Orchestration - Repository Guide"));
    assert!(!content.contains("Some stale guide content"));
    assert!(!content.contains("Generated by Loom Installation Process"));
    assert!(!content.contains("Old pointer text"));
}

#[test]
fn test_setup_scaffolding_upgrades_marker_first_hybrid_root_claude_md() {
    // Robustness variant of the #3476 Bug 1 fix: legacy content AFTER the
    // marker block (marker-block-first hybrid) must also trigger full
    // replacement — the `after` slice is checked too.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "CLAUDE.md",
        "# Loom Orchestration - Repository Guide\n\nFull guide content (new).",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    let hybrid =
        format!("{}\n\n{}\n", wrap_loom_content("Old pointer text"), LEGACY_ROOT_CLAUDE_MD);
    fs::write(workspace.join("CLAUDE.md"), &hybrid).unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), "upgrade of marker-first hybrid failed: {result:?}");

    let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
    // Trailing newline is normalized on write (issue #6331).
    assert_eq!(content, format!("{}\n", wrap_loom_content(LOOM_ROOT_POINTER)));
    assert!(!content.contains("{{LOOM_VERSION}}"));
    assert!(!content.contains("Generated by Loom Installation Process"));
}

#[test]
fn test_slice_is_discardable_legacy_distinguishes_shapes() {
    // Regression unit test for #3527. The slice-discard predicate must
    // separate a genuine legacy guide fragment from a long-lived consumer
    // file that merely starts with a legacy-looking header line.

    // A genuine legacy guide fragment carries multiple signatures — discard.
    assert!(slice_is_discardable_legacy(LEGACY_ROOT_CLAUDE_MD));

    // A short slice with a single signature is still legacy cruft — discard.
    assert!(slice_is_discardable_legacy(
        "# Loom Orchestration - Repository Guide\n\nA few lines of stale guide text.\n"
    ));

    // The bucket-brigade shape: one legacy-looking header line followed by
    // hundreds of lines of real consumer content. Exactly ONE signature in a
    // large slice must be PRESERVED, not discarded.
    let bulk: String = (0..1000)
        .map(|i| format!("Real consumer line {i} with unique content.\n"))
        .collect();
    let consumer = format!("# Loom Orchestration - Repository Guide\n\n{bulk}");
    assert!(
        !slice_is_discardable_legacy(&consumer),
        "large consumer slice with one surviving header line must be preserved"
    );

    // No signatures at all => always preserve.
    assert!(!slice_is_discardable_legacy(
        "# My Project\n\nHand-written docs with no Loom signatures.\n"
    ));

    // Empty slice => preserve (nothing to discard).
    assert!(!slice_is_discardable_legacy(""));
}

#[test]
fn test_setup_scaffolding_preserves_large_consumer_content_with_legacy_header() {
    // Regression test for #3527 (bucket-brigade PR #480 data loss).
    //
    // Shape: a root CLAUDE.md that (a) starts with the legacy-looking header
    // `# Loom Orchestration - Repository Guide` as a plain heading, (b) has
    // ~1000 lines of unrelated real consumer content between that heading and
    // the marker block, and (c) has a valid modern marker block near the end.
    //
    // Before the fix, `is_legacy_loom_managed_root(before)` returned true
    // because the slice contained the header signature, collapsing the entire
    // file to the 3-line pointer stub and deleting ~1000 lines of consumer
    // content. After the fix, the outside-marker content must be preserved
    // byte-for-byte and only the marker-delimited section may change.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "CLAUDE.md",
        "# Loom Orchestration - Repository Guide\n\nNew full guide content.",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    // Build the consumer content: legacy-looking header + ~1000 lines of real,
    // organically-added content that must survive verbatim.
    let bulk: String = (0..1000)
        .map(|i| {
            format!("Remote-dev guide line {i}: Anvil integration reference detail number {i}.\n")
        })
        .collect();
    let consumer_before = format!(
        "# Loom Orchestration - Repository Guide\n\n\
             ## Compute Resource Guidelines\n\n\
             NEVER train models locally. Use the remote host inventory.\n\n\
             {bulk}"
    );

    // Assemble the full file: consumer content, then an OLD marker block
    // (pre-existing pointer) that the installer will refresh in place.
    let pre_existing = format!(
        "{}\n{}\n\n## Trailing Consumer Section\n\nMore real content after the markers.\n",
        consumer_before,
        wrap_loom_content("Old pointer text"),
    );
    fs::write(workspace.join("CLAUDE.md"), &pre_existing).unwrap();

    let mut report = InitReport::default();
    let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
    assert!(result.is_ok(), "upgrade of large consumer CLAUDE.md failed: {result:?}");

    let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

    // The consumer content outside the markers must survive byte-for-byte.
    assert!(
        content.contains("## Compute Resource Guidelines"),
        "consumer section header lost: data loss regression"
    );
    assert!(content.contains("NEVER train models locally. Use the remote host inventory."));
    assert!(content.contains("## Trailing Consumer Section"));
    assert!(content.contains("More real content after the markers."));
    // Spot-check the bulk lines survived.
    assert!(content.contains("Remote-dev guide line 0:"));
    assert!(content.contains("Remote-dev guide line 999:"));
    for i in [0usize, 250, 500, 750, 999] {
        assert!(
            content.contains(&format!(
                "Remote-dev guide line {i}: Anvil integration reference detail number {i}."
            )),
            "consumer line {i} was deleted"
        );
    }

    // The marker section was refreshed to the new pointer, exactly once.
    assert!(content.contains(LOOM_ROOT_POINTER));
    assert!(!content.contains("Old pointer text"));
    assert_eq!(content.matches(LOOM_SECTION_START).count(), 1);
    assert_eq!(content.matches(LOOM_SECTION_END).count(), 1);

    // The file did NOT collapse to the bare stub.
    assert_ne!(
        content,
        wrap_loom_content(LOOM_ROOT_POINTER),
        "file collapsed to bare pointer stub — consumer content was deleted"
    );
}

#[test]
fn test_setup_scaffolding_preserves_modern_marker_block() {
    // Regression guard for #3000 behavior: an existing modern marker block
    // must update the wrapped pointer in place, not replace the whole file.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "CLAUDE.md",
        "# Loom Orchestration - Repository Guide\n\nNew full guide content.",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    // Pre-existing modern root with markers + project content above and below.
    let pre_existing = format!(
        "# My Project\n\nIntro paragraph.\n\n{}\n{}\n{}\n\n## Project Notes\n\nMore stuff.\n",
        LOOM_SECTION_START, "Old pointer text", LOOM_SECTION_END
    );
    fs::write(workspace.join("CLAUDE.md"), &pre_existing).unwrap();

    let mut report = InitReport::default();
    setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

    let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

    // User content above and below the marker block must survive.
    assert!(content.contains("# My Project"));
    assert!(content.contains("Intro paragraph"));
    assert!(content.contains("## Project Notes"));
    assert!(content.contains("More stuff"));
    // The Loom section is updated to the new pointer.
    assert!(content.contains(LOOM_ROOT_POINTER));
    // Old marker contents are gone.
    assert!(!content.contains("Old pointer text"));
}

#[test]
fn test_setup_scaffolding_preserves_user_content_without_legacy_signature() {
    // Genuine user-authored root CLAUDE.md (no markers, no Loom signatures)
    // must be preserved with the marker block appended at the end.
    let temp_dir = TempDir::new().unwrap();
    let (workspace, defaults) = setup_test_with_template(
        &temp_dir,
        "CLAUDE.md",
        "# Loom Orchestration - Repository Guide\n\nNew full guide content.",
    );
    fs::create_dir_all(workspace.join(".loom")).unwrap();

    let user_content = "\
# My Awesome Project

This project does amazing things.

## Getting Started

Run `cargo run` to start.";
    fs::write(workspace.join("CLAUDE.md"), user_content).unwrap();

    let mut report = InitReport::default();
    setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

    let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

    // User content survives.
    assert!(content.contains("My Awesome Project"));
    assert!(content.contains("amazing things"));
    assert!(content.contains("Getting Started"));
    assert!(content.contains("cargo run"));
    // Loom marker block is appended.
    assert!(content.contains(LOOM_SECTION_START));
    assert!(content.contains(LOOM_ROOT_POINTER));
    // User content comes BEFORE the marker block.
    let user_pos = content.find("My Awesome Project").unwrap();
    let loom_pos = content.find(LOOM_SECTION_START).unwrap();
    assert!(user_pos < loom_pos, "user content must precede Loom block");
}

#[test]
fn test_assert_no_placeholders_catches_corrupt_template() {
    // Defense-in-depth: if a future code path slips literal `{{LOOM_VERSION}}`
    // past the substitution step, the post-write assertion must reject it.
    // We exercise the assertion directly here; the install path itself can't
    // currently produce a leaked-placeholder file (the legacy branch
    // replaces with a hardcoded pointer; the marker branch reuses the same
    // string), but the guard is the safety net.
    let leaky = format!("{LOOM_SECTION_START}\n{{{{LOOM_VERSION}}}}\n{LOOM_SECTION_END}");
    let err = assert_no_placeholders(&leaky, "CLAUDE.md").unwrap_err();
    assert!(err.contains("CLAUDE.md"));
    assert!(err.contains("{{LOOM_VERSION}}"));

    // Sanity check: the normal install output (just the wrapped pointer)
    // must pass.
    let clean = wrap_loom_content(LOOM_ROOT_POINTER);
    assert!(assert_no_placeholders(&clean, "CLAUDE.md").is_ok());
}

// ── .github/labels.yml Loom-block merge (issue #4187) ──────────────────

const SHIPPED_LABELS: &str = "# BEGIN LOOM LABELS\n# managed by Loom\n- name: loom:issue\n  color: \"3B82F6\"\n- name: loom:building\n  color: \"F59E0B\"\n# END LOOM LABELS\n";

#[test]
fn test_labels_block_range_well_formed() {
    let (start, end) = labels_block_range(SHIPPED_LABELS).unwrap();
    assert_eq!(&SHIPPED_LABELS[start..start + LOOM_LABELS_START.len()], LOOM_LABELS_START);
    assert!(SHIPPED_LABELS[..end].ends_with(LOOM_LABELS_END));
}

#[test]
fn test_labels_block_range_absent_or_malformed() {
    // No markers at all.
    assert!(labels_block_range("- name: team:foo\n  color: abcdef\n").is_none());
    // END before BEGIN is not a valid block.
    let inverted = "# END LOOM LABELS\n- name: x\n# BEGIN LOOM LABELS\n";
    // BEGIN is found, but END only searched after BEGIN -> none.
    assert!(labels_block_range(inverted).is_none());
}

#[test]
fn test_merge_labels_block_replaces_marked_range_preserving_outside() {
    // Consumer file: labels above and below a stale Loom block.
    let existing = "- name: team:above\n  color: \"111111\"\n\n# BEGIN LOOM LABELS\n- name: loom:issue\n  color: \"000000\"\n# END LOOM LABELS\n\n- name: team:below\n  color: \"222222\"\n";
    let (merged, _absorbed) = merge_labels_block(existing, SHIPPED_LABELS).unwrap();

    // Consumer entries outside the block are byte-preserved.
    assert!(merged.contains("- name: team:above"));
    assert!(merged.contains("- name: team:below"));
    // The Loom block is refreshed to the shipped content.
    assert!(merged.contains("color: \"3B82F6\""));
    assert!(merged.contains("- name: loom:building"));
    assert!(!merged.contains("color: \"000000\""), "stale Loom color must be gone");
    // Exactly one marker pair.
    assert_eq!(merged.matches(LOOM_LABELS_START).count(), 1);
    assert_eq!(merged.matches(LOOM_LABELS_END).count(), 1);
}

#[test]
fn test_merge_labels_block_appends_to_markerless_file() {
    let existing = "- name: team:frontend\n  color: \"00ff00\"\n  description: consumer label\n";
    let (merged, _absorbed) = merge_labels_block(existing, SHIPPED_LABELS).unwrap();

    // Every existing entry survives.
    assert!(merged.contains("- name: team:frontend"));
    assert!(merged.contains("description: consumer label"));
    // The Loom block is appended.
    assert!(merged.contains(LOOM_LABELS_START));
    assert!(merged.contains("- name: loom:issue"));
    // Consumer content precedes the appended block.
    assert!(merged.find("team:frontend").unwrap() < merged.find(LOOM_LABELS_START).unwrap());
}

#[test]
fn test_merge_labels_block_noop_when_block_matches() {
    // A file that already equals a plain copy of the shipped file needs no change.
    assert!(merge_labels_block(SHIPPED_LABELS, SHIPPED_LABELS).is_none());
}

#[test]
fn test_merge_labels_block_preserves_when_source_markerless() {
    // Defensive: a shipped file without markers must never clobber consumer content.
    let existing = "- name: team:only\n  color: \"abcdef\"\n";
    assert!(merge_labels_block(existing, "- name: loom:issue\n  color: fff\n").is_none());
}

#[test]
fn test_install_labels_block_fresh_install_is_verbatim_copy() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("labels.src.yml");
    let dst = temp.path().join("labels.yml");
    fs::write(&src, SHIPPED_LABELS).unwrap();

    let mut report = InitReport::default();
    install_labels_block(&src, &dst, None, &mut report).unwrap();

    // Fresh install ships the file byte-for-byte (keeps registry parity).
    assert_eq!(fs::read_to_string(&dst).unwrap(), SHIPPED_LABELS);
    assert!(report.added.contains(&LABELS_YML_REL.to_string()));
    assert!(!report.preserved.contains(&LABELS_YML_REL.to_string()));
}

#[test]
fn test_install_labels_block_force_restores_consumer_content() {
    // Simulate a --force directory copy having clobbered the dst with the
    // shipped file, while pre_existing captured the consumer's real content.
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("labels.src.yml");
    let dst = temp.path().join("labels.yml");
    fs::write(&src, SHIPPED_LABELS).unwrap();
    // Post-clobber on-disk state.
    fs::write(&dst, SHIPPED_LABELS).unwrap();

    let pre_existing = "# BEGIN LOOM LABELS\n- name: loom:issue\n  color: \"000000\"\n# END LOOM LABELS\n\n- name: team:below\n  color: \"222222\"\n";

    let mut report = InitReport::default();
    install_labels_block(&src, &dst, Some(pre_existing), &mut report).unwrap();

    let result = fs::read_to_string(&dst).unwrap();
    // Consumer label survives the force reinstall.
    assert!(result.contains("- name: team:below"));
    // Loom block refreshed.
    assert!(result.contains("color: \"3B82F6\""));
    // Recorded as preserved (consumer-owned) — dropped any copy-side entry.
    assert!(report.preserved.contains(&LABELS_YML_REL.to_string()));
    assert_eq!(report.added.iter().filter(|f| *f == LABELS_YML_REL).count(), 0);
    assert_eq!(
        report
            .updated
            .iter()
            .filter(|f| *f == LABELS_YML_REL)
            .count(),
        0
    );
}
