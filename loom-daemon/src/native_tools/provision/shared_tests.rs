#![allow(clippy::unwrap_used)]
use super::*;
use crate::native_tools::provision::reap;
use std::collections::BTreeSet;

fn workspace_root(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".git"), "fixture git marker").unwrap();
    root
}

fn trees(workspace: &Path) -> Vec<PathBuf> {
    fs::read_dir(workspace.join(reap::BINDINGS))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect()
}

/// The acceptance case from #8663: two launches of the same workspace provision
/// ONE binding tree, and the second inherits the (126 MB, in production) package
/// tree the first installed instead of building its own.
#[test]
fn two_launches_for_one_workspace_share_a_single_binding_tree() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace_root(temp.path(), "repo");
    let base = temp.path().join("external");

    let one = state::create(&root, Some(&base), None, None).unwrap();
    let two = state::create(&root, Some(&base), None, None).unwrap();
    assert_ne!(one.directory, two.directory, "sessions stay isolated");
    assert_eq!(one.workspace, two.workspace);

    let first = opencode_config_dir(&one.workspace).unwrap();
    assert_eq!(
        fs::read_to_string(first.join("package.json")).unwrap(),
        super::super::OPENCODE_PLUGIN_MANIFEST
    );
    assert!(first.join("plugins/loom.ts").is_file());
    // Stand in for the CLI's own plugin install, which is what costs the space.
    fs::create_dir_all(first.join("node_modules/@opencode-ai/plugin")).unwrap();

    let second = opencode_config_dir(&two.workspace).unwrap();
    assert_eq!(first, second);
    assert!(
        second.join("node_modules/@opencode-ai/plugin").is_dir(),
        "the second launch reuses the installed package tree"
    );
    assert_eq!(trees(&one.workspace).len(), 1, "exactly one provisioned tree");
    for session in [&one.directory, &two.directory] {
        assert!(
            !session.join("opencode").exists(),
            "no per-session copy of the binding tree is created"
        );
    }
}

/// Distinct workspaces never share a tree, and the shared tree is not inside
/// any session directory the reaper will remove.
#[test]
fn workspaces_are_separate_and_trees_live_outside_session_directories() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("external");
    let a = state::create(&workspace_root(temp.path(), "one"), Some(&base), None, None).unwrap();
    let b = state::create(&workspace_root(temp.path(), "two"), Some(&base), None, None).unwrap();
    let first = opencode_config_dir(&a.workspace).unwrap();
    let second = opencode_config_dir(&b.workspace).unwrap();
    assert_ne!(first, second);
    assert!(first.starts_with(a.workspace.join(reap::BINDINGS)));
    assert!(!first.starts_with(&a.directory));
    // Same content, same key, different workspace parent.
    assert_eq!(
        first.strip_prefix(&a.workspace).unwrap(),
        second.strip_prefix(&b.workspace).unwrap()
    );
}

/// Simultaneous cold launches converge on one tree and leave no staging debris.
#[test]
fn concurrent_cold_launches_publish_exactly_one_tree() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace_root(temp.path(), "repo");
    let base = temp.path().join("external");
    let workspace = state::create(&root, Some(&base), None, None)
        .unwrap()
        .workspace;
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let workspace = workspace.clone();
            std::thread::spawn(move || opencode_config_dir(&workspace).unwrap())
        })
        .collect();
    let paths: BTreeSet<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(paths.len(), 1);
    assert_eq!(trees(&workspace).len(), 1);
    assert_eq!(
        fs::read_dir(workspace.join(reap::STAGING))
            .into_iter()
            .flatten()
            .count(),
        0,
        "no staging tree is left behind"
    );
}

/// A tree whose bindings were damaged is repaired in place on the next launch,
/// rather than serving a manifest no launch wrote.
#[test]
fn a_damaged_tree_is_self_healed_and_marked_used() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace_root(temp.path(), "repo");
    let base = temp.path().join("external");
    let workspace = state::create(&root, Some(&base), None, None)
        .unwrap()
        .workspace;
    let config_dir = opencode_config_dir(&workspace).unwrap();
    fs::write(config_dir.join("package.json"), "{\"tampered\":true}").unwrap();
    fs::remove_file(config_dir.join("plugins/loom.ts")).unwrap();
    let again = opencode_config_dir(&workspace).unwrap();
    assert_eq!(again, config_dir);
    assert_eq!(
        fs::read_to_string(config_dir.join("package.json")).unwrap(),
        super::super::OPENCODE_PLUGIN_MANIFEST
    );
    assert!(config_dir.join("plugins/loom.ts").is_file());
    assert!(
        config_dir.parent().unwrap().join(reap::LAST_USED).is_file(),
        "each launch refreshes the idle marker the reaper ages"
    );
}

#[test]
fn the_binding_key_is_content_addressed_and_stable() {
    let key = binding_key();
    assert_eq!(key.len(), 64);
    assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(key, binding_key());
}
