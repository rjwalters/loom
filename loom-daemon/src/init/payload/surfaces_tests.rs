//! Tests for the surfaces beyond the installer step (#10895), on temp
//! workspaces only: a small synthetic `defaults/` per case, plus guards that
//! read the real templates and retired list.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use super::super::{materialize_with, resync_workspace_with, Payload, ResyncOutcome, Stamp};
use super::*;
use crate::install_compat::{Version, INSTALL_METADATA_PATH};

const META: &str = INSTALL_METADATA_PATH;
const DATE: &str = "2026-01-02";

fn stamp() -> Stamp {
    Stamp {
        version: Version::parse("0.19.990").unwrap(),
        commit: Some("b".repeat(40)),
        requires_daemon: "0.19.772".to_string(),
        release_build: true,
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// The repo's real `defaults/` tree.
fn real_defaults() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults")
}

/// A `defaults/` tree with two generated skills, the single-file docs, a
/// retired list and both guide templates.
fn fake_defaults(root: &Path) -> PathBuf {
    let d = root.join("defaults");
    write(&d.join(".loom-README.md"), "readme\n");
    write(&d.join("pricing.json"), "{}\n");
    for role in ["builder", "judge"] {
        let body = format!("# {role}\n\nThe {role} role.\n");
        write(&d.join(format!("roles/{role}.md")), &body);
        write(&d.join(format!(".claude/commands/loom/{role}.md")), &body);
    }
    write(&d.join("scripts/a.sh"), "#!/bin/sh\n");
    write(&d.join(".claude/README.md"), "claude readme v2\n");
    write(&d.join(".github/CONFIGURATION.md"), "configuration v2\n");
    write(&d.join(".claude/biome.jsonc"), "{\"v\": 2}\n");
    write(
        &d.join(".loom-retired.list"),
        "# retired\nscripts/old.sh  # gone\nscripts/pinned.sh\nscripts/linked.sh\n\
         commands/loom/old.md\n../escape.sh\n",
    );
    write(&d.join(".loom/CLAUDE.md"), &guide_template("claude", 1));
    write(&d.join(".loom/AGENTS.md"), &guide_template("agents", 1));
    d
}

fn guide_template(name: &str, rev: u32) -> String {
    format!(
        "# {name} guide rev {rev}\n\n**Installation Date**: {{{{INSTALL_DATE}}}}\n\n\
         See [docs](.loom/docs/x.md).\n\nLast updated: {{{{INSTALL_DATE}}}}\n"
    )
}

fn rendered_guide(name: &str, rev: u32, date: &str) -> String {
    guide_template(name, rev)
        .replace("{{INSTALL_DATE}}", date)
        .replace("](.loom/", "](")
}

fn generated_skill(defaults: &Path, name: &str) -> String {
    crate::agent_skills::generate_all(defaults)
        .unwrap()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap()
        .content
}

/// A workspace with every surface current for `defaults`.
fn current_workspace(root: &Path, defaults: &Path) -> PathBuf {
    let ws = root.join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    write(
        &ws.join(META),
        "{\n  \"loom_version\": \"0.19.900\",\n  \"installed_files\": []\n}\n",
    );
    let mut report = crate::init::InitReport::default();
    crate::init::install_payload_files(&ws, defaults, &ws.join(".loom"), true, &mut report)
        .unwrap();
    for role in ["builder", "judge"] {
        let rel = format!(".claude/commands/loom/{role}.md");
        write(&ws.join(&rel), &read(&defaults.join(&rel)));
        write(
            &ws.join(format!(".agents/skills/loom-{role}/SKILL.md")),
            &generated_skill(defaults, role),
        );
    }
    for rel in [
        ".claude/README.md",
        ".github/CONFIGURATION.md",
        ".claude/biome.jsonc",
    ] {
        write(&ws.join(rel), &read(&defaults.join(rel)));
    }
    write(&ws.join(".loom/CLAUDE.md"), &rendered_guide("claude", 1, DATE));
    write(&ws.join(".loom/AGENTS.md"), &rendered_guide("agents", 1, DATE));
    crate::init::update_gitignore(&ws).unwrap();
    // Repo-owned files no resync may touch.
    write(&ws.join(".loom/config.json"), "{\"repo\": true}\n");
    write(&ws.join("package.json"), "{\"name\": \"loom-workspace\", \"version\": \"1\"}\n");
    write(&ws.join("CLAUDE.md"), "**Loom Version**: 0.1\nrepo guide\n");
    write(&ws.join(".gitattributes"), "*.bin binary\n");
    ws
}

fn payload(defaults: &Path) -> Payload {
    Payload::from_defaults(defaults.to_path_buf(), stamp())
}

fn diff_paths(p: &Payload, ws: &Path) -> (Vec<String>, Vec<String>, Vec<String>) {
    let d = materialize_with(p, ws).unwrap();
    (d.added.clone(), d.changed.clone(), d.removed.clone())
}

fn empty() -> (Vec<String>, Vec<String>, Vec<String>) {
    (vec![], vec![], vec![])
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// Bytes and mtime of every file under `root`, mtimes first pinned to a
/// fixed past instant so any write shows.
fn freeze(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let mut files = Vec::new();
    walk(root, &mut files);
    for f in &files {
        fs::File::options()
            .write(true)
            .open(f)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }
    snapshot(root)
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut files = Vec::new();
    walk(root, &mut files);
    files
        .into_iter()
        .map(|f| {
            let v = (fs::read(&f).unwrap(), fs::metadata(&f).unwrap().modified().unwrap());
            (f, v)
        })
        .collect()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let ft = entry.file_type().unwrap();
        if ft.is_dir() {
            walk(&entry.path(), out);
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
}

fn installed_files(ws: &Path) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(&read(&ws.join(META))).unwrap();
    v["installed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_current_workspace_is_an_empty_diff_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    let before = freeze(&ws);
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
    assert_eq!(
        resync_workspace_with(&payload(&defaults), &ws).unwrap(),
        ResyncOutcome::Unchanged
    );
    assert_eq!(before, snapshot(&ws));
}

#[test]
fn skills_are_marker_gated_backfilled_and_retired() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    let skill = |n: &str| format!(".agents/skills/loom-{n}/SKILL.md");
    // Stale but Loom's: updated.
    let stale = generated_skill(&defaults, "builder").replace("The builder role.", "old");
    write(&ws.join(skill("builder")), &stale);
    // Absent: backfilled.
    fs::remove_file(ws.join(skill("judge"))).unwrap();
    // Marker-carrying, no longer generated: removed.
    write(&ws.join(skill("gone")), &format!("{}\nold\n", crate::agent_skills::MARKER));
    // Consumer-authored, at a `loom-` name and at another: never touched.
    write(&ws.join(skill("custom")), "my own skill\n");
    write(&ws.join(".agents/skills/notes/SKILL.md"), "not loom's\n");

    let p = payload(&defaults);
    let (added, changed, removed) = diff_paths(&p, &ws);
    assert_eq!(added, vec![skill("judge")]);
    assert_eq!(changed, vec![skill("builder")]);
    assert_eq!(removed, vec![skill("gone")]);

    let outcome = resync_workspace_with(&p, &ws).unwrap();
    assert!(matches!(outcome, ResyncOutcome::Applied { .. }), "{outcome:?}");
    assert_eq!(read(&ws.join(skill("builder"))), generated_skill(&defaults, "builder"));
    assert_eq!(read(&ws.join(skill("judge"))), generated_skill(&defaults, "judge"));
    assert!(!ws.join(".agents/skills/loom-gone").exists());
    assert_eq!(read(&ws.join(skill("custom"))), "my own skill\n");
    assert_eq!(read(&ws.join(".agents/skills/notes/SKILL.md")), "not loom's\n");
    // The backfilled and updated skills are now Loom's to retire later.
    let listed = installed_files(&ws);
    assert!(
        listed.contains(&skill("judge")) && listed.contains(&skill("builder")),
        "{listed:?}"
    );
    assert_eq!(diff_paths(&p, &ws), empty());
}

#[test]
fn a_skill_without_the_marker_is_never_written() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    let path = ws.join(".agents/skills/loom-builder/SKILL.md");
    write(&path, "detached by the consumer\n");
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
    // Not UTF-8 is not "absent".
    fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
}

#[test]
fn a_payload_whose_skills_cannot_be_generated_removes_none() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    // A role with no description: generation fails as a whole.
    write(&defaults.join("roles/broken.md"), "no heading\n");
    write(&defaults.join(".claude/commands/loom/broken.md"), "no heading\n");
    let (_, _, removed) = diff_paths(&payload(&defaults), &ws);
    assert!(removed.iter().all(|p| !p.starts_with(".agents/")), "{removed:?}");
}

#[test]
fn single_file_docs_update_when_present_and_biome_backfills() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    write(&ws.join(".claude/README.md"), "claude readme v1\n");
    fs::remove_file(ws.join(".github/CONFIGURATION.md")).unwrap();
    fs::remove_file(ws.join(".claude/biome.jsonc")).unwrap();

    let (added, changed, removed) = diff_paths(&payload(&defaults), &ws);
    assert_eq!(added, strings(&[".claude/biome.jsonc"]));
    assert_eq!(changed, strings(&[".claude/README.md"]));
    assert!(removed.is_empty());
    resync_workspace_with(&payload(&defaults), &ws).unwrap();
    assert!(!ws.join(".github/CONFIGURATION.md").exists(), "never created");
    assert_eq!(read(&ws.join(".claude/README.md")), "claude readme v2\n");
}

#[cfg(unix)]
#[test]
fn retired_sweep_removes_unrecorded_files_and_respects_pins_and_symlinks() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    write(&ws.join(".loom/scripts/old.sh"), "retired, never recorded\n");
    write(&ws.join(".loom/scripts/pinned.sh"), "kept by pin\n");
    write(&ws.join(".claude/commands/loom/old.md"), "kept by the script's pin form\n");
    write(&ws.join(".loom/resync-ignore"), "scripts/pinned.sh\ncommands/loom/old.md\n");
    let elsewhere = tmp.path().join("elsewhere.sh");
    write(&elsewhere, "outside\n");
    std::os::unix::fs::symlink(&elsewhere, ws.join(".loom/scripts/linked.sh")).unwrap();
    write(&ws.join(".loom/scripts/stray.sh"), "unlisted, repo's\n");
    write(&tmp.path().join("escape.sh"), "outside the repo\n");

    let (added, changed, removed) = diff_paths(&payload(&defaults), &ws);
    assert!(added.is_empty() && changed.is_empty(), "{added:?} {changed:?}");
    assert_eq!(removed, strings(&[".loom/scripts/old.sh"]));
    resync_workspace_with(&payload(&defaults), &ws).unwrap();
    assert!(!ws.join(".loom/scripts/old.sh").exists());
    assert!(ws.join(".loom/scripts/pinned.sh").exists());
    assert!(ws.join(".claude/commands/loom/old.md").exists());
    assert!(ws.join(".loom/scripts/linked.sh").is_symlink());
    assert_eq!(read(&elsewhere), "outside\n");
    assert!(ws.join(".loom/scripts/stray.sh").exists());
    assert!(tmp.path().join("escape.sh").exists());
}

#[test]
fn gitignore_block_is_merged_never_recorded_and_never_created() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    let current = read(&ws.join(".gitignore"));
    let begin = current.find("# >>> loom-managed").unwrap();
    let end = current.find("# <<< loom-managed").unwrap();
    let stale = format!(
        "node_modules/\n{}.loom/only-old-pattern\n{}\n# after\ndist/\n",
        &current[begin..end],
        &current[end..].trim_end()
    );
    write(&ws.join(".gitignore"), &stale);

    let (added, changed, removed) = diff_paths(&payload(&defaults), &ws);
    assert_eq!((added, changed, removed), (vec![], strings(&[".gitignore"]), vec![]));
    resync_workspace_with(&payload(&defaults), &ws).unwrap();
    let merged = read(&ws.join(".gitignore"));
    assert!(merged.starts_with("node_modules/\n# >>> loom-managed"), "{merged}");
    assert!(merged.ends_with("\n# after\ndist/\n"), "{merged}");
    assert!(!merged.contains("only-old-pattern"));
    assert!(!installed_files(&ws).contains(&".gitignore".to_string()));
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());

    fs::remove_file(ws.join(".gitignore")).unwrap();
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
}

#[test]
fn guides_keep_their_install_date_and_change_only_with_the_template() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    // A different install date, same template: nothing to do.
    write(&ws.join(".loom/CLAUDE.md"), &rendered_guide("claude", 1, "2025-05-05"));
    write(&ws.join(".loom/AGENTS.md"), &rendered_guide("agents", 1, "2025-06-06"));
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());

    // A changed template: re-rendered, each keeping its own date.
    write(&defaults.join(".loom/CLAUDE.md"), &guide_template("claude", 2));
    write(&defaults.join(".loom/AGENTS.md"), &guide_template("agents", 2));
    let (_, changed, _) = diff_paths(&payload(&defaults), &ws);
    assert_eq!(changed, strings(&[".loom/AGENTS.md", ".loom/CLAUDE.md"]));
    resync_workspace_with(&payload(&defaults), &ws).unwrap();
    assert_eq!(read(&ws.join(".loom/CLAUDE.md")), rendered_guide("claude", 2, "2025-05-05"));
    assert_eq!(read(&ws.join(".loom/AGENTS.md")), rendered_guide("agents", 2, "2025-06-06"));
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
}

#[test]
fn guides_are_never_created_and_need_a_recorded_date() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    fs::remove_file(ws.join(".loom/AGENTS.md")).unwrap();
    write(&ws.join(".loom/CLAUDE.md"), "hand-written, no date line\n");
    write(&defaults.join(".loom/CLAUDE.md"), &guide_template("claude", 2));
    write(&defaults.join(".loom/AGENTS.md"), &guide_template("agents", 2));
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
    assert!(!ws.join(".loom/AGENTS.md").exists());
}

#[test]
fn a_template_needing_another_value_leaves_the_guide_alone() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    write(
        &defaults.join(".loom/CLAUDE.md"),
        &format!("{}\nVersion {{{{LOOM_VERSION}}}}\n", guide_template("claude", 2)),
    );
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
}

#[test]
fn a_full_resync_touches_no_repo_owned_file_and_a_second_one_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    let repo_owned = [
        ".loom/config.json",
        "package.json",
        "CLAUDE.md",
        ".gitattributes",
    ];
    let kept: Vec<String> = repo_owned.iter().map(|p| read(&ws.join(p))).collect();
    // Every covered surface stale at once.
    write(&ws.join(".agents/skills/loom-builder/SKILL.md"), crate::agent_skills::MARKER);
    write(&ws.join(".claude/README.md"), "old\n");
    write(&ws.join(".github/CONFIGURATION.md"), "old\n");
    fs::remove_file(ws.join(".claude/biome.jsonc")).unwrap();
    write(&ws.join(".gitignore"), "mine/\n");
    write(&ws.join(".loom/scripts/old.sh"), "retired\n");
    write(&defaults.join(".loom/CLAUDE.md"), &guide_template("claude", 3));
    write(&defaults.join(".loom/AGENTS.md"), &guide_template("agents", 3));

    let outcome = resync_workspace_with(&payload(&defaults), &ws).unwrap();
    let ResyncOutcome::Applied { written } = outcome else {
        panic!("{outcome:?}");
    };
    for p in [
        ".agents/skills/loom-builder/SKILL.md",
        ".claude/README.md",
        ".github/CONFIGURATION.md",
        ".claude/biome.jsonc",
        ".gitignore",
        ".loom/scripts/old.sh",
        ".loom/CLAUDE.md",
        ".loom/AGENTS.md",
    ] {
        assert!(written.contains(&p.to_string()), "{p} not in {written:?}");
    }
    for (p, before) in repo_owned.iter().zip(&kept) {
        assert_eq!(&read(&ws.join(p)), before, "{p} must be byte-identical");
    }

    let before = freeze(&ws);
    assert_eq!(
        resync_workspace_with(&payload(&defaults), &ws).unwrap(),
        ResyncOutcome::Unchanged
    );
    assert_eq!(before, snapshot(&ws), "a second resync writes nothing, metadata included");
}

#[cfg(unix)]
#[test]
fn a_symlinked_parent_of_an_extra_surface_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    let outside = tmp.path().join("shared-agents");
    fs::rename(ws.join(".agents"), &outside).unwrap();
    fs::remove_file(outside.join("skills/loom-judge/SKILL.md")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join(".agents")).unwrap();
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
    assert!(!outside.join("skills/loom-judge/SKILL.md").exists());
}

#[test]
fn pins_hold_in_the_scripts_report_form() {
    assert_eq!(
        pin_names(".agents/skills/loom-x/SKILL.md"),
        strings(&[
            ".agents/skills/loom-x/SKILL.md",
            "agents-skills/loom-x/SKILL.md"
        ])
    );
    assert_eq!(
        pin_names(".claude/commands/loom/x.md"),
        strings(&[".claude/commands/loom/x.md", "commands/loom/x.md"])
    );
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = current_workspace(tmp.path(), &defaults);
    write(&ws.join(".agents/skills/loom-builder/SKILL.md"), crate::agent_skills::MARKER);
    write(&ws.join(".loom/resync-ignore"), "agents-skills/loom-builder/SKILL.md\n");
    assert_eq!(diff_paths(&payload(&defaults), &ws), empty());
}

#[test]
fn recorded_install_date_reads_the_rendered_line() {
    assert_eq!(
        recorded_install_date("x\n**Installation Date**: 2026-03-04\n"),
        Some("2026-03-04")
    );
    assert_eq!(recorded_install_date("**Installation Date**: {{INSTALL_DATE}}\n"), None);
    assert_eq!(recorded_install_date("**Installation Date**:   \n"), None);
    assert_eq!(recorded_install_date("no line\n"), None);
}

/// A guide is re-rendered only because its template's one placeholder is
/// the install date, read back from the installed file. A second placeholder
/// would make every resync of every repo stop refreshing that guide; fail
/// here instead, so the change is made deliberately.
#[test]
fn the_real_guide_templates_hold_only_the_install_date_placeholder() {
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let template = read(&real_defaults().join(".loom").join(name));
        assert!(template.contains(INSTALL_DATE_PLACEHOLDER), "{name}");
        assert!(template.contains(INSTALL_DATE_LINE), "{name}");
        assert_eq!(unreproducible_placeholders(&template), Vec::<String>::new(), "{name}");
    }
    assert_eq!(
        unreproducible_placeholders("{{A_B}} {{INSTALL_DATE}} {{x}} {{"),
        vec!["{{A_B}}"]
    );
}

/// `init`'s rendering of the real templates is reproduced exactly from the
/// date it wrote, so an unchanged release is an empty diff.
#[test]
fn the_real_guides_rendered_by_init_are_an_empty_diff() {
    let tmp = TempDir::new().unwrap();
    let defaults = real_defaults();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let meta = super::super::super::templates::LoomMetadata {
            version: Some("9.9.9".into()),
            commit: Some("c0ffee".into()),
            install_date: "2024-12-31".into(),
            requires_daemon: None,
        };
        let template = read(&defaults.join(".loom").join(name));
        let rendered = render_dotloom_guide(&template, Some("o"), Some("r"), &meta);
        write(&ws.join(".loom").join(name), &rendered);
    }
    let staged = tmp.path().join("stage");
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let dst = staged.join(".loom").join(name);
        write(&dst, &read(&ws.join(".loom").join(name)));
        assert!(stage_guide(&defaults.join(".loom").join(name), &dst).unwrap());
        assert_eq!(read(&dst), read(&ws.join(".loom").join(name)), "{name}");
    }
}

#[test]
fn retired_targets_map_like_the_script_and_stay_inside_the_export() {
    assert_eq!(retired_target("scripts/a.sh").as_deref(), Some(".loom/scripts/a.sh"));
    assert_eq!(retired_target("bin/loom").as_deref(), Some(".loom/bin/loom"));
    assert_eq!(
        retired_target("commands/loom/x.md").as_deref(),
        Some(".claude/commands/loom/x.md")
    );
    assert_eq!(
        retired_target(".github/CONFIGURATION.md").as_deref(),
        Some(".github/CONFIGURATION.md")
    );
    for bad in ["other/x", "scripts/../x", "../x", "scripts//x", ""] {
        assert_eq!(retired_target(bad), None, "{bad}");
    }
    let specs = export_pathspecs();
    let list = read(&real_defaults().join(RETIRED_LIST));
    let entries: Vec<&str> = retired_entries(&list).collect();
    assert!(entries.len() >= 10, "{entries:?}");
    for entry in entries {
        let target = retired_target(entry).unwrap_or_else(|| panic!("{entry} maps nowhere"));
        assert!(specs.iter().any(|s| within(&target, s)), "{target} is not exported");
    }
}

#[test]
fn every_extra_surface_is_exported_and_none_is_install_time_only() {
    let specs = export_pathspecs();
    for surface in EXTRA_SURFACES {
        assert!(specs.iter().any(|s| within(surface.path, s)), "{}", surface.path);
        assert!(
            INSTALL_TIME_ONLY.iter().all(|(p, _)| *p != surface.path),
            "{} is both covered and install-time only",
            surface.path
        );
    }
    assert_eq!(specs[..2], [".loom", ".claude/commands/loom"]);
    assert!(!specs.contains(&".loom/CLAUDE.md"), "already inside .loom");
}
