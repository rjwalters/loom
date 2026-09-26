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

// --- Dogfood arm (issue #8947) ------------------------------------------
//
// When the install target IS the loom source repo, `.agents/skills` must be a
// symlink into `defaults/`, never a copy — the #3565/#3682 lesson applied to
// the fourth surface in that family. The fixtures below put `defaults/`
// directly under the workspace, which is exactly the path shape
// `install_agent_skills` keys the dogfood branch off.

/// Dogfood layout: `workspace/defaults/...`, so `defaults.parent() == workspace`.
fn make_dogfood_layout(temp: &Path) -> std::path::PathBuf {
    let defaults = temp.join("defaults");
    make_defaults_with_one_role(&defaults);
    // `generate_all` only writes into `defaults/.agents/skills` when something
    // asks it to; the dogfood arm links to that directory, so it must exist.
    fs::create_dir_all(defaults.join(".agents/skills/loom-foo")).unwrap();
    fs::write(
        defaults.join(".agents/skills/loom-foo/SKILL.md"),
        format!("---\nname: loom-foo\n---\n{}\nbody\n", crate::agent_skills::MARKER),
    )
    .unwrap();
    defaults
}

#[test]
fn dogfood_install_symlinks_instead_of_copying() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = make_dogfood_layout(workspace);

    let mut report = InitReport::default();
    install_agent_skills(&defaults, workspace, &mut report).unwrap();

    let link = workspace.join(".agents/skills");
    let target = fs::read_link(&link).expect("dogfood install must produce a symlink");
    assert_eq!(
        target,
        Path::new("../defaults/.agents/skills"),
        "symlink must point at the shipped source of truth: {report:?}"
    );
    // The linked content must resolve — a symlink to nowhere is worse than a copy.
    assert!(link.join("loom-foo/SKILL.md").exists());
}

#[test]
fn dogfood_install_is_idempotent() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = make_dogfood_layout(workspace);

    let mut report = InitReport::default();
    install_agent_skills(&defaults, workspace, &mut report).unwrap();
    let mut second = InitReport::default();
    install_agent_skills(&defaults, workspace, &mut second).unwrap();

    assert_eq!(
        fs::read_link(workspace.join(".agents/skills")).unwrap(),
        Path::new("../defaults/.agents/skills")
    );
    assert!(
        second
            .preserved
            .iter()
            .any(|p| p.contains(".agents/skills")),
        "a correct symlink must be preserved, not rewritten: {second:?}"
    );
    assert!(second.added.is_empty(), "no re-add on second run: {second:?}");
}

#[test]
fn dogfood_install_replaces_a_stale_materialized_copy() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = make_dogfood_layout(workspace);

    // The exact state this issue was filed about: a real directory holding a
    // drifted copy of defaults/, untracked and un-ignored.
    let copy = workspace.join(".agents/skills/loom-foo");
    fs::create_dir_all(&copy).unwrap();
    fs::write(copy.join("SKILL.md"), "stale copy").unwrap();

    let mut report = InitReport::default();
    install_agent_skills(&defaults, workspace, &mut report).unwrap();

    assert_eq!(
        fs::read_link(workspace.join(".agents/skills")).unwrap(),
        Path::new("../defaults/.agents/skills"),
        "a stale copy fully covered by defaults/ must be replaced: {report:?}"
    );
    assert!(!fs::read_to_string(workspace.join(".agents/skills/loom-foo/SKILL.md"))
        .unwrap()
        .contains("stale copy"));
}

#[test]
fn dogfood_install_refuses_to_discard_local_only_files() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = make_dogfood_layout(workspace);

    // A human-authored skill that defaults/ knows nothing about. Replacing the
    // directory with a symlink would silently delete it, so the install must
    // back off — same guard as `link_dogfood_commands`.
    let local = workspace.join(".agents/skills/hand-written");
    fs::create_dir_all(&local).unwrap();
    fs::write(local.join("SKILL.md"), "local only").unwrap();

    let mut report = InitReport::default();
    install_agent_skills(&defaults, workspace, &mut report).unwrap();

    assert!(local.join("SKILL.md").exists(), "local-only work must survive: {report:?}");
    assert!(
        fs::read_link(workspace.join(".agents/skills")).is_err(),
        "must not have been replaced by a symlink: {report:?}"
    );
    assert!(
        report.preserved.iter().any(|p| p.contains("local-only")),
        "the back-off must be reported, not silent: {report:?}"
    );
}

#[test]
fn non_dogfood_install_still_copies() {
    // Guard against the dogfood path-shape test over-matching: a consumer repo
    // (defaults/ NOT directly under the workspace) must keep getting real files,
    // because consumer repos track `.agents/skills/`.
    let temp_dir = TempDir::new().unwrap();
    let defaults = temp_dir.path().join("defaults");
    let workspace = temp_dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    make_defaults_with_one_role(&defaults);

    let mut report = InitReport::default();
    install_agent_skills(&defaults, &workspace, &mut report).unwrap();

    let dst = workspace.join(".agents/skills/loom-foo/SKILL.md");
    assert!(dst.is_file(), "consumer install must write real files: {report:?}");
    assert!(fs::read_link(workspace.join(".agents/skills")).is_err());
}
