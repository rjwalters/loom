//! Unit tests for `--retire-aliases` (#9152).
//!
//! Every case that retires something also asserts the main workspace's
//! `node_modules` canary survives: "unlink, not delete" is the property, and
//! a retire that took the target with it would pass a symlink-gone check.

use std::os::unix::fs::symlink;

use super::*;

/// A throwaway directory that removes itself.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("loom-wt-retire-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp tree");
        Self(path)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A main workspace with an installed root `node_modules` (canary inside)
/// and a worktree directory at the layout `worktree.sh` uses. `pnpm` adds the
/// lockfile marker.
fn workspace(tree: &TempTree, pnpm: bool) -> (PathBuf, PathBuf) {
    let main = tree.0.join("main");
    fs::create_dir_all(main.join("node_modules")).unwrap();
    fs::write(main.join("node_modules/CANARY.txt"), "root dep\n").unwrap();
    fs::write(main.join("package.json"), r#"{"name":"root"}"#).unwrap();
    if pnpm {
        fs::write(main.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    }
    let worktree = main.join(".loom/worktrees/issue-1");
    fs::create_dir_all(&worktree).unwrap();
    (main, worktree)
}

fn opts(main: &Path, worktree: Option<&Path>) -> RetireOptions {
    RetireOptions {
        repo_root: main.to_path_buf(),
        worktree: worktree.map(Path::to_path_buf),
        quiet: true,
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

fn canary_survives(main: &Path) -> bool {
    main.join("node_modules/CANARY.txt").is_file()
}

#[test]
fn retires_a_root_alias_into_the_main_workspace_and_keeps_the_target() {
    let tree = TempTree::new("root");
    let (main, worktree) = workspace(&tree, true);
    symlink(main.join("node_modules"), worktree.join("node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(!is_symlink(&worktree.join("node_modules")), "alias must be gone");
    assert!(!worktree.join("node_modules").exists());
    assert!(canary_survives(&main), "unlink, not delete: main node_modules must survive");
}

#[test]
fn retires_a_nested_per_package_alias() {
    let tree = TempTree::new("nested");
    let (main, worktree) = workspace(&tree, true);
    fs::create_dir_all(main.join("apps/web/node_modules")).unwrap();
    fs::write(main.join("apps/web/node_modules/CANARY.txt"), "web dep\n").unwrap();
    fs::create_dir_all(worktree.join("apps/web")).unwrap();
    symlink(main.join("apps/web/node_modules"), worktree.join("apps/web/node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(!is_symlink(&worktree.join("apps/web/node_modules")));
    assert!(main.join("apps/web/node_modules/CANARY.txt").is_file());
}

#[test]
fn a_real_node_modules_directory_is_left_untouched() {
    let tree = TempTree::new("realdir");
    let (main, worktree) = workspace(&tree, true);
    fs::create_dir_all(worktree.join("node_modules/pkg")).unwrap();
    fs::write(worktree.join("node_modules/pkg/index.js"), "own install\n").unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(worktree.join("node_modules/pkg/index.js").is_file());
    assert!(canary_survives(&main));
}

#[test]
fn a_symlink_pointing_outside_the_main_workspace_is_left_untouched() {
    let tree = TempTree::new("elsewhere");
    let (main, worktree) = workspace(&tree, true);
    let elsewhere = tree.0.join("elsewhere/node_modules");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, worktree.join("node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(is_symlink(&worktree.join("node_modules")));
}

#[test]
fn a_link_into_the_worktree_itself_or_a_dangling_link_is_left_untouched() {
    let tree = TempTree::new("self");
    let (main, worktree) = workspace(&tree, true);
    fs::create_dir_all(worktree.join("vendor/modules")).unwrap();
    symlink(worktree.join("vendor/modules"), worktree.join("node_modules")).unwrap();
    fs::create_dir_all(worktree.join("apps/web")).unwrap();
    symlink(main.join("does-not-exist"), worktree.join("apps/web/node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(is_symlink(&worktree.join("node_modules")));
    assert!(is_symlink(&worktree.join("apps/web/node_modules")));
}

#[test]
fn a_non_pnpm_repo_is_untouched() {
    let tree = TempTree::new("npm");
    let (main, worktree) = workspace(&tree, false);
    symlink(main.join("node_modules"), worktree.join("node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(is_symlink(&worktree.join("node_modules")), "npm/yarn links are not the hazard");
}

#[test]
fn an_explicit_link_node_modules_opt_in_is_respected() {
    let tree = TempTree::new("optin");
    let (main, worktree) = workspace(&tree, true);
    fs::create_dir_all(main.join(".loom")).unwrap();
    fs::write(main.join(".loom/config.json"), r#"{"worktree":{"linkNodeModules":true}}"#).unwrap();
    symlink(main.join("node_modules"), worktree.join("node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&worktree))), 0);

    assert!(is_symlink(&worktree.join("node_modules")));
}

#[test]
fn naming_the_main_workspace_as_the_worktree_touches_nothing() {
    let tree = TempTree::new("main-as-wt");
    let (main, _worktree) = workspace(&tree, true);
    fs::create_dir_all(main.join("apps/web")).unwrap();
    // A link in the main tree that WOULD qualify if main were a worktree.
    symlink(main.join("node_modules"), main.join("apps/web/node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, Some(&main))), 0);

    assert!(is_symlink(&main.join("apps/web/node_modules")));
    assert!(canary_survives(&main));
}

#[test]
fn unlink_symlink_refuses_a_real_directory() {
    let tree = TempTree::new("refuse");
    let dir = tree.0.join("node_modules");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("keep.txt"), "x").unwrap();

    assert!(unlink_symlink(&dir).is_err());
    assert!(dir.join("keep.txt").is_file());
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

#[test]
fn without_worktree_every_listed_worktree_is_covered() {
    let tree = TempTree::new("all");
    let (main, _) = workspace(&tree, true);
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let wt_a = tree.0.join("wt-a");
    let wt_b = tree.0.join("wt-b");
    git(&main, &["worktree", "add", "-q", "--detach", wt_a.to_str().unwrap()]);
    git(&main, &["worktree", "add", "-q", "--detach", wt_b.to_str().unwrap()]);
    symlink(main.join("node_modules"), wt_a.join("node_modules")).unwrap();
    symlink(main.join("node_modules"), wt_b.join("node_modules")).unwrap();

    assert_eq!(retire_aliases(&opts(&main, None)), 0);

    assert!(!is_symlink(&wt_a.join("node_modules")));
    assert!(!is_symlink(&wt_b.join("node_modules")));
    assert!(canary_survives(&main));
}
