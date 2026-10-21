use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::super::surfaces::SkipReason;
use super::super::{materialize_with, resync_workspace_with, Payload, ResyncOutcome, Stamp};
use super::{locate, resolve_relative, RootPath};
use crate::install_compat::{Version, INSTALL_METADATA_PATH};

#[test]
fn a_root_with_no_link_above_it_is_direct() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".claude/commands/loom")).unwrap();
    assert_eq!(locate(tmp.path(), ".claude/commands/loom"), RootPath::Direct);
}

#[test]
fn a_link_into_the_repo_resolves_to_the_real_path() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".agents/commands")).unwrap();
    fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    symlink("../.agents/commands", tmp.path().join(".claude/commands")).unwrap();
    assert_eq!(
        locate(tmp.path(), ".claude/commands/loom"),
        RootPath::InRepo(".agents/commands/loom".to_string())
    );
}

#[test]
fn a_link_out_of_the_repo_a_dangling_link_and_a_loop_are_unreachable() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(tmp.path().join("elsewhere")).unwrap();

    symlink("../../elsewhere", repo.join(".claude/commands")).unwrap();
    assert!(matches!(
        locate(&repo, ".claude/commands/loom"),
        RootPath::Unreachable { link, why } if link == ".claude/commands" && why.contains("outside")
    ));

    fs::remove_file(repo.join(".claude/commands")).unwrap();
    symlink(tmp.path().join("elsewhere"), repo.join(".claude/commands")).unwrap();
    assert!(matches!(
        locate(&repo, ".claude/commands/loom"),
        RootPath::Unreachable { why, .. } if why.contains("outside")
    ));

    fs::remove_file(repo.join(".claude/commands")).unwrap();
    symlink("../.agents/nowhere", repo.join(".claude/commands")).unwrap();
    assert!(matches!(
        locate(&repo, ".claude/commands/loom"),
        RootPath::Unreachable { why, .. } if why.contains("dangling")
    ));

    fs::remove_file(repo.join(".claude/commands")).unwrap();
    symlink("commands", repo.join(".claude/commands")).unwrap();
    assert!(matches!(locate(&repo, ".claude/commands/loom"), RootPath::Unreachable { .. }));
}

#[test]
fn a_git_recorded_link_resolves_lexically() {
    assert_eq!(
        resolve_relative(".claude/commands", "../.agents/commands").as_deref(),
        Some(".agents/commands")
    );
    assert_eq!(resolve_relative(".claude/commands", "../../out"), None);
    assert_eq!(resolve_relative(".claude/commands", "/abs/path"), None);
}

// ---- the resync over a linked root, against the real `defaults/` --------

const COMMANDS: &str = ".claude/commands/loom";

fn payload() -> Payload {
    let defaults: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults");
    Payload::from_defaults(
        defaults,
        Stamp {
            version: Version::parse("0.19.990").unwrap(),
            commit: Some("b".repeat(40)),
            requires_daemon: "0.19.772".to_string(),
            release_build: true,
        },
    )
}

fn workspace(root: &Path) -> PathBuf {
    let ws = root.join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(ws.join(".loom")).unwrap();
    fs::create_dir_all(ws.join(".claude")).unwrap();
    fs::write(
        ws.join(INSTALL_METADATA_PATH),
        "{\n  \"loom_version\": \"0.19.900\",\n  \"installed_files\": []\n}\n",
    )
    .unwrap();
    ws
}

fn metadata(ws: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(ws.join(INSTALL_METADATA_PATH)).unwrap()).unwrap()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// miniset's layout: `.claude/commands -> ../.agents/commands`.
#[test]
fn a_root_linked_inside_the_repo_is_resynced_at_its_real_path() {
    let tmp = TempDir::new().unwrap();
    let ws = workspace(tmp.path());
    fs::create_dir_all(ws.join(".agents/commands")).unwrap();
    fs::write(ws.join(".agents/commands/mine.md"), "mine\n").unwrap();
    symlink("../.agents/commands", ws.join(".claude/commands")).unwrap();

    let diff = materialize_with(&payload(), &ws).unwrap();
    let prompts: Vec<&String> = diff
        .added
        .iter()
        .filter(|p| p.starts_with(".agents/commands/loom/"))
        .collect();
    assert!(!prompts.is_empty(), "the role prompts are added at the real path");
    assert!(
        !diff
            .added
            .iter()
            .any(|p| p.starts_with(".claude/commands/")),
        "no diff path goes through the link: {:?}",
        diff.added
    );
    assert!(diff.held_roots().is_empty());

    let outcome = resync_workspace_with(&payload(), &ws).unwrap();
    let ResyncOutcome::Applied { written } = outcome else {
        panic!("{outcome:?}");
    };
    assert!(written
        .iter()
        .any(|p| p.starts_with(".agents/commands/loom/")));
    assert!(!written.iter().any(|p| p.starts_with(".claude/commands/")), "{written:?}");
    assert!(fs::symlink_metadata(ws.join(".claude/commands"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(ws.join(".agents/commands/mine.md")).unwrap(), "mine\n");
    let meta = metadata(&ws);
    assert_eq!(meta["loom_version"], "0.19.990");
    assert!(meta.get("resync_held").is_none());
    let listed: Vec<&str> = meta["installed_files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        listed
            .iter()
            .any(|p| p.starts_with(".claude/commands/loom/")),
        "logical paths"
    );
    assert!(!listed.iter().any(|p| p.starts_with(".agents/commands/")), "{listed:?}");

    assert!(
        materialize_with(&payload(), &ws).unwrap().is_empty(),
        "the second pass is a no-op"
    );
}

#[test]
fn a_root_linked_out_of_the_repo_is_skipped_reported_and_holds_the_stamp() {
    let tmp = TempDir::new().unwrap();
    let ws = workspace(tmp.path());
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, ws.join(".claude/commands")).unwrap();

    let diff = materialize_with(&payload(), &ws).unwrap();
    assert_eq!(diff.held_roots(), [COMMANDS.to_string()]);
    assert!(diff.skipped_surfaces.iter().any(|s| s.path == COMMANDS
        && matches!(&s.why, SkipReason::BehindSymlink(why) if why.contains("outside"))));
    assert!(!diff
        .added
        .iter()
        .chain(&diff.changed)
        .any(|p| p.starts_with(".claude/commands/")));

    resync_workspace_with(&payload(), &ws).unwrap();
    assert!(files_under(&elsewhere).is_empty(), "nothing is written through the link");
    let meta = metadata(&ws);
    assert_eq!(meta["loom_version"], "0.19.900", "the stamp does not claim the prompts");
    assert_eq!(meta["resync_held"]["version"], "0.19.990");
    assert_eq!(meta["resync_held"]["roots"][0], COMMANDS);
    assert!(meta.get("resync_pending").is_none());
    assert!(materialize_with(&payload(), &ws).unwrap().is_empty(), "and it stays quiet");

    // The link is replaced by a real directory: the next resync writes the
    // prompts and the stamp catches up.
    fs::remove_file(ws.join(".claude/commands")).unwrap();
    fs::create_dir_all(ws.join(".claude/commands")).unwrap();
    resync_workspace_with(&payload(), &ws).unwrap();
    assert!(!files_under(&ws.join(COMMANDS)).is_empty());
    let meta = metadata(&ws);
    assert_eq!(meta["loom_version"], "0.19.990");
    assert!(meta.get("resync_held").is_none());
}

#[test]
fn a_root_behind_a_dangling_link_is_skipped_and_nothing_is_created() {
    let tmp = TempDir::new().unwrap();
    let ws = workspace(tmp.path());
    symlink("../.agents/nowhere", ws.join(".claude/commands")).unwrap();

    let diff = materialize_with(&payload(), &ws).unwrap();
    assert_eq!(diff.held_roots(), [COMMANDS.to_string()]);
    assert!(diff.skipped_surfaces.iter().any(|s| s.path == COMMANDS
        && matches!(&s.why, SkipReason::BehindSymlink(why) if why.contains("dangling"))));
    resync_workspace_with(&payload(), &ws).unwrap();
    assert!(!ws.join(".agents/nowhere").exists(), "the link's target is not created");
    assert_eq!(metadata(&ws)["loom_version"], "0.19.900");
}
