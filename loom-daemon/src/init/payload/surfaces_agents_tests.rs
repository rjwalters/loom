//! Tests for the `.claude/agents/` surface (#11033), against the real
//! `defaults/` tree and a bare temp workspace; only the agents paths of each
//! diff are asserted on.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::super::{materialize_with, resync_workspace_with, Payload, Stamp};
use crate::install_compat::{Version, INSTALL_METADATA_PATH};

const CHAMPION: &str = ".claude/agents/loom-champion.md";

fn payload() -> Payload {
    let defaults: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults");
    let stamp = Stamp {
        version: Version::parse("0.19.990").unwrap(),
        commit: Some("b".repeat(40)),
        requires_daemon: "0.19.772".to_string(),
        release_build: true,
    };
    Payload::from_defaults(defaults, stamp)
}

fn workspace(root: &Path) -> PathBuf {
    let ws = root.join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(ws.join(".loom")).unwrap();
    fs::write(
        ws.join(INSTALL_METADATA_PATH),
        "{\n  \"loom_version\": \"0.19.900\",\n  \"installed_files\": []\n}\n",
    )
    .unwrap();
    ws
}

fn agents_only(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| p.starts_with(".claude/agents"))
        .cloned()
        .collect()
}

#[test]
fn a_stale_agent_is_updated_and_a_repo_own_agent_is_kept() {
    let tmp = TempDir::new().unwrap();
    let ws = workspace(tmp.path());
    fs::create_dir_all(ws.join(".claude/agents")).unwrap();
    fs::write(ws.join(CHAMPION), "stale\n").unwrap();
    fs::write(ws.join(".claude/agents/mine.md"), "mine\n").unwrap();

    let diff = materialize_with(&payload(), &ws).unwrap();
    assert_eq!(agents_only(&diff.changed), vec![CHAMPION.to_string()]);
    assert!(
        agents_only(&diff.added).len() > 1 && !agents_only(&diff.added).contains(&CHAMPION.into()),
        "the other shipped agents are backfilled into the existing directory"
    );
    assert!(agents_only(&diff.removed).is_empty(), "mine.md is never removed");

    resync_workspace_with(&payload(), &ws).unwrap();
    let shipped = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../defaults")
            .join(CHAMPION),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(ws.join(CHAMPION)).unwrap(), shipped);
    assert_eq!(fs::read_to_string(ws.join(".claude/agents/mine.md")).unwrap(), "mine\n");
    let again = materialize_with(&payload(), &ws).unwrap();
    assert!(agents_only(&again.added).is_empty() && agents_only(&again.changed).is_empty());
}

#[test]
fn a_workspace_without_an_agents_directory_is_not_given_one() {
    let tmp = TempDir::new().unwrap();
    let ws = workspace(tmp.path());
    let diff = materialize_with(&payload(), &ws).unwrap();
    assert!(agents_only(&diff.added).is_empty());
    resync_workspace_with(&payload(), &ws).unwrap();
    assert!(!ws.join(".claude/agents").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_agents_directory_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    let ws = workspace(tmp.path());
    let target = tmp.path().join("shared-agents");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("loom-champion.md"), "mine\n").unwrap();
    fs::create_dir_all(ws.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&target, ws.join(".claude/agents")).unwrap();

    let diff = materialize_with(&payload(), &ws).unwrap();
    for list in [&diff.added, &diff.changed, &diff.removed] {
        assert!(agents_only(list).is_empty(), "{list:?}");
    }
    resync_workspace_with(&payload(), &ws).unwrap();
    assert_eq!(fs::read_to_string(target.join("loom-champion.md")).unwrap(), "mine\n");
}
