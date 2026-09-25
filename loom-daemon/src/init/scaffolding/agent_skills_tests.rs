//! Tests for `install_agent_skills` (issue #8673) — the install-time
//! counterpart of `defaults/scripts/tests/test-resync-installed-agent-skills-guard.sh`,
//! which covers the same marker contract for the resync path.

use super::*;
use tempfile::TempDir;

/// Minimal `defaults/roles/` + `defaults/.claude/commands/loom/` fixture: one
/// role ("foo") with a JSON sidecar, discoverable by
/// `crate::agent_skills::generate_all`.
fn make_defaults_with_one_role(defaults: &Path) {
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join(".claude/commands/loom")).unwrap();
    fs::write(defaults.join("roles/foo.md"), "symlink-placeholder").unwrap();
    fs::write(defaults.join("roles/foo.json"), r#"{"description":"Does foo things"}"#).unwrap();
    fs::write(
        defaults.join(".claude/commands/loom/foo.md"),
        "# Foo Role\n\nDoes foo things.\n",
    )
    .unwrap();
}

#[test]
fn install_agent_skills_creates_when_absent() {
    let temp_dir = TempDir::new().unwrap();
    let defaults = temp_dir.path().join("defaults");
    let workspace = temp_dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    make_defaults_with_one_role(&defaults);

    let mut report = InitReport::default();
    install_agent_skills(&defaults, &workspace, &mut report).unwrap();

    let dst = workspace.join(".agents/skills/loom-foo/SKILL.md");
    assert!(dst.exists(), "SKILL.md must be created: {report:?}");
    let content = fs::read_to_string(&dst).unwrap();
    assert!(content.contains(crate::agent_skills::MARKER));
    assert!(content.contains("name: loom-foo"));
    assert!(
        report
            .added
            .iter()
            .any(|p| p == ".agents/skills/loom-foo/SKILL.md"),
        "expected an 'added' report entry: {report:?}"
    );
}

#[test]
fn install_agent_skills_updates_when_marker_present_and_stale() {
    let temp_dir = TempDir::new().unwrap();
    let defaults = temp_dir.path().join("defaults");
    let workspace = temp_dir.path().join("workspace");
    make_defaults_with_one_role(&defaults);

    let dst_dir = workspace.join(".agents/skills/loom-foo");
    fs::create_dir_all(&dst_dir).unwrap();
    fs::write(
        dst_dir.join("SKILL.md"),
        format!(
            "---\nname: loom-foo\ndescription: \"stale\"\n---\n{}\nstale body\n",
            crate::agent_skills::MARKER
        ),
    )
    .unwrap();

    let mut report = InitReport::default();
    install_agent_skills(&defaults, &workspace, &mut report).unwrap();

    let content = fs::read_to_string(dst_dir.join("SKILL.md")).unwrap();
    assert!(content.contains("Does foo things"));
    assert!(
        report
            .updated
            .iter()
            .any(|p| p == ".agents/skills/loom-foo/SKILL.md"),
        "expected an 'updated' report entry: {report:?}"
    );
}

#[test]
fn install_agent_skills_preserves_markerless_destination() {
    let temp_dir = TempDir::new().unwrap();
    let defaults = temp_dir.path().join("defaults");
    let workspace = temp_dir.path().join("workspace");
    make_defaults_with_one_role(&defaults);

    let dst_dir = workspace.join(".agents/skills/loom-foo");
    fs::create_dir_all(&dst_dir).unwrap();
    let hand_authored = "Hand-authored content — not Loom-generated.\n";
    fs::write(dst_dir.join("SKILL.md"), hand_authored).unwrap();

    let mut report = InitReport::default();
    install_agent_skills(&defaults, &workspace, &mut report).unwrap();

    let content = fs::read_to_string(dst_dir.join("SKILL.md")).unwrap();
    assert_eq!(
        content, hand_authored,
        "a destination file with no ownership marker must be left byte-for-byte untouched"
    );
    assert!(
        report
            .preserved
            .iter()
            .any(|p| p == ".agents/skills/loom-foo/SKILL.md"),
        "expected a 'preserved' report entry: {report:?}"
    );
    assert!(
        !report
            .updated
            .iter()
            .any(|p| p == ".agents/skills/loom-foo/SKILL.md"),
        "must not also report the markerless file as updated: {report:?}"
    );
}

#[test]
fn install_agent_skills_is_silent_noop_without_roles_dir() {
    // A defaults/ tree with no roles/ directory at all (e.g. an unusually
    // old or minimal fixture) must not fail the whole scaffolding pass —
    // .agents/skills/ is an optional surface.
    let temp_dir = TempDir::new().unwrap();
    let defaults = temp_dir.path().join("defaults");
    let workspace = temp_dir.path().join("workspace");
    fs::create_dir_all(&defaults).unwrap();
    fs::create_dir_all(&workspace).unwrap();

    let mut report = InitReport::default();
    let result = install_agent_skills(&defaults, &workspace, &mut report);
    assert!(result.is_ok(), "missing roles/ must not error: {result:?}");
    assert!(!workspace.join(".agents").exists());
}
