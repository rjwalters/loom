//! Unit tests for the symlink-provisioning port (#8195 slice 4).
//!
//! The retained suite (`defaults/scripts/tests/test-worktree-nested-symlinks.sh`)
//! and the differential harness (`tests/worktree_link_differential.rs`) are the
//! equivalence evidence. These are the cases those two cannot reach cheaply:
//! the bash test-operator semantics one predicate at a time, and the path
//! shapes (`/`-absolute, space-bearing, non-UTF-8) that a shell fixture cannot
//! express without becoming a test of the fixture.

use super::*;

/// A throwaway directory that removes itself. `std::env::temp_dir()` + pid +
/// a counter, so parallel test threads never collide.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("loom-wt-link-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp tree");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn file(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir -p");
        }
        fs::write(&p, contents).expect("write");
        p
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// concat vs Path::join — the containment property
// ---------------------------------------------------------------------------

#[test]
// `clippy::join_absolute_paths` fires on the two `base.join("/etc/ssh")` calls
// below, and it is right about what they do — which is the point. They are not
// production code doing the wrong thing; they are this test *demonstrating*
// the hazard `concat` exists to avoid, by asserting exactly what `Path::join`
// would have answered. Silencing the assertion instead of the lint would
// delete the comparison that gives `concat` its reason to exist.
#[allow(clippy::join_absolute_paths)]
fn concat_keeps_an_absolute_entry_inside_the_base() {
    // The whole point: `Path::join` would answer `/etc/ssh` here. The shell
    // answered `"$base//etc/ssh"`, which exists only if someone built that
    // path inside the workspace. A `worktree.linkPaths` entry must never be
    // able to reach out of the workspace, and this is what stops it.
    let base = Path::new("/main/workspace");
    let concatenated = concat(base, OsStr::new("/etc/ssh"));
    assert_eq!(concatenated, PathBuf::from("/main/workspace//etc/ssh"));
    assert!(concatenated.starts_with(base));
    assert_ne!(concatenated, base.join("/etc/ssh"));
    assert_eq!(base.join("/etc/ssh"), PathBuf::from("/etc/ssh"));
}

#[test]
fn concat_preserves_spaces_and_shell_metacharacters() {
    let base = Path::new("/main work space");
    let entry = OsStr::new("a dir/$(touch pwned)/x");
    assert_eq!(concat(base, entry), PathBuf::from("/main work space/a dir/$(touch pwned)/x"));
}

// ---------------------------------------------------------------------------
// ExcludeFile — `grep -qxF` + `echo >>`
// ---------------------------------------------------------------------------

/// Build an `ExcludeFile` pointing at an arbitrary path, bypassing the git
/// resolution (which the differential harness covers end to end).
fn exclude_at(path: &Path) -> ExcludeFile {
    ExcludeFile {
        path: Some(path.to_path_buf()),
    }
}

#[test]
fn append_creates_the_file_and_is_idempotent() {
    let tree = TempTree::new("excl-idem");
    let target = tree.path().join("info/exclude");
    let excl = exclude_at(&target);

    excl.append(OsStr::new("node_modules"));
    excl.append(OsStr::new("node_modules"));
    excl.append(OsStr::new(".mcp.json"));
    excl.append(OsStr::new("node_modules"));

    let body = fs::read_to_string(&target).expect("exclude file written");
    assert_eq!(body, "node_modules\n.mcp.json\n");
}

#[test]
fn append_matches_whole_lines_only() {
    // `grep -qxF` is anchored at both ends: `node_modules` must NOT be
    // considered present because `apps/web/node_modules` is. Getting this
    // wrong silently drops the root exclude entry — which is exactly the
    // #5474 defect, in a different disguise.
    let tree = TempTree::new("excl-whole");
    let target = tree.file("exclude", "apps/web/node_modules\nnode_modules_x\n");
    let excl = exclude_at(&target);

    excl.append(OsStr::new("node_modules"));

    let body = fs::read_to_string(&target).unwrap();
    assert_eq!(body, "apps/web/node_modules\nnode_modules_x\nnode_modules\n");
}

#[test]
fn append_dedupes_against_a_non_utf8_exclude_file() {
    // A lossy read would fail to find the existing line and append a
    // duplicate. The shell's `grep -qxF` compares bytes; so does this.
    let tree = TempTree::new("excl-bytes");
    let target = tree.path().join("exclude");
    {
        let mut f = fs::File::create(&target).unwrap();
        f.write_all(&[0xff, 0xfe, b'\n']).unwrap();
        f.write_all(b"node_modules\n").unwrap();
    }
    let excl = exclude_at(&target);
    excl.append(OsStr::new("node_modules"));

    let body = fs::read(&target).unwrap();
    assert_eq!(body, b"\xff\xfe\nnode_modules\n");
}

#[test]
fn append_is_a_no_op_when_git_had_no_answer() {
    // The shell returned immediately on an empty $WORKTREE_INFO_EXCLUDE.
    // Nothing must be created, anywhere.
    let tree = TempTree::new("excl-none");
    let excl = ExcludeFile { path: None };
    excl.append(OsStr::new("node_modules"));
    assert!(fs::read_dir(tree.path()).unwrap().next().is_none());
}

#[test]
fn resolve_answers_none_for_a_missing_worktree() {
    let tree = TempTree::new("excl-missing");
    let resolved = ExcludeFile::resolve(&tree.path().join("not-there"));
    assert!(resolved.path.is_none());
}

// ---------------------------------------------------------------------------
// discover_nested_node_modules — the `find` predicate, term by term
// ---------------------------------------------------------------------------

fn rels(root: &Path, found: &[PathBuf]) -> Vec<String> {
    found
        .iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn scan_honours_mindepth_maxdepth_and_the_node_modules_ancestor_exclusion() {
    let tree = TempTree::new("scan");
    tree.dir("node_modules"); // depth 1 — below -mindepth 2
    tree.dir("apps/web/node_modules"); // depth 3 — in range
    tree.dir("packages/node_modules"); // depth 2 — in range
    tree.dir("a/b/c/node_modules"); // depth 4 — beyond -maxdepth 3
    tree.dir("node_modules/.pnpm/node_modules"); // ancestor is node_modules
    tree.dir("apps/web/src"); // not a node_modules at all

    let found = discover_nested_node_modules(tree.path());
    assert_eq!(
        rels(tree.path(), &found),
        vec!["apps/web/node_modules", "packages/node_modules"],
    );
}

#[test]
fn scan_result_is_sorted_so_message_order_is_deterministic() {
    let tree = TempTree::new("scan-sorted");
    for name in ["zeta", "alpha", "middle"] {
        tree.dir(&format!("{name}/node_modules"));
    }
    let found = discover_nested_node_modules(tree.path());
    assert_eq!(
        rels(tree.path(), &found),
        vec![
            "alpha/node_modules",
            "middle/node_modules",
            "zeta/node_modules"
        ],
    );
}

#[test]
fn scan_does_not_follow_a_symlink_to_a_directory() {
    // `find` defaults to -P, so a symlinked package dir is `-type l`, not
    // `-type d`, and is neither reported nor descended into.
    let tree = TempTree::new("scan-symlink");
    tree.dir("real/node_modules");
    symlink(tree.path().join("real"), tree.path().join("mirror")).unwrap();

    let found = discover_nested_node_modules(tree.path());
    assert_eq!(rels(tree.path(), &found), vec!["real/node_modules"]);
}

#[test]
fn scan_finds_packages_whose_directory_names_contain_spaces() {
    // #7858's class, in this family's own shape: the shell survived it only
    // because of `-print0` plus a quoted `read -r -d ''`.
    let tree = TempTree::new("scan spaces");
    tree.dir("apps/my web app/node_modules");
    let found = discover_nested_node_modules(tree.path());
    assert_eq!(rels(tree.path(), &found), vec!["apps/my web app/node_modules"]);
}

#[test]
fn has_node_modules_ancestor_tests_the_whole_path_not_just_the_scan_subtree() {
    // The prune inside `scan` cannot see a `node_modules` component that is
    // part of the scan ROOT. `find` could, because `-path` matched the full
    // path it printed, so this predicate is applied explicitly too.
    assert!(has_node_modules_ancestor(Path::new("/srv/node_modules/app/node_modules")));
    assert!(!has_node_modules_ancestor(Path::new("/srv/app/node_modules")));
    assert!(!has_node_modules_ancestor(Path::new("/node_modules")));
}

// ---------------------------------------------------------------------------
// configured_link_paths — jq's semantics
// ---------------------------------------------------------------------------

/// The machine-level defaults tier needs no suppression here:
/// `config_resolver::private_defaults_path` refuses its home fallback under
/// `cfg(test)` (#8584), so an operator's live `defaults.json` cannot decide
/// what these fixtures say. The differential harness, which spawns the
/// BINARY, does have to pass `LOOM_CONFIG_DEFAULTS_FILE=""` — and does.
fn link_paths_from(json: &str) -> Vec<String> {
    let tree = TempTree::new("cfg");
    tree.file(".loom/config.json", json);
    configured_link_paths(tree.path())
}

#[test]
fn link_paths_reads_an_array_of_strings() {
    assert_eq!(
        link_paths_from(r#"{"worktree":{"linkPaths":["apps/web/src/wasm","a b/c"]}}"#),
        vec!["apps/web/src/wasm".to_string(), "a b/c".to_string()],
    );
}

#[test]
fn link_paths_is_empty_when_the_key_is_absent_or_not_iterable() {
    assert!(link_paths_from(r#"{}"#).is_empty());
    assert!(link_paths_from(r#"{"worktree":{}}"#).is_empty());
    // `.worktree.linkPaths[]?` emits nothing rather than erroring on a scalar.
    assert!(link_paths_from(r#"{"worktree":{"linkPaths":"apps/web"}}"#).is_empty());
    assert!(link_paths_from(r#"{"worktree":"nope"}"#).is_empty());
}

#[test]
fn link_paths_drops_the_values_jq_considers_falsy() {
    // `// empty` drops exactly `null` and `false`, and nothing else.
    assert_eq!(
        link_paths_from(r#"{"worktree":{"linkPaths":[null,"keep",false,0,""]}}"#),
        vec!["keep".to_string(), "0".to_string(), String::new()],
    );
}

#[test]
fn link_paths_iterates_an_objects_values_the_way_jq_does() {
    assert_eq!(
        link_paths_from(r#"{"worktree":{"linkPaths":{"a":"one","b":"two"}}}"#),
        vec!["one".to_string(), "two".to_string()],
    );
}

// ---------------------------------------------------------------------------
// End-to-end over a fixture tree
// ---------------------------------------------------------------------------

/// Read a symlink's target, asserting the path really is one.
fn link_target(path: &Path) -> PathBuf {
    assert!(
        fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "expected a symlink at {path:?}"
    );
    fs::read_link(path).expect("read_link")
}

#[test]
fn run_provisions_every_family_through_paths_containing_spaces() {
    // The #7858 regression case the acceptance criteria call for, covering
    // this slice's own surface rather than relying on the retained suite:
    // EVERY path in play contains a space, and one contains shell
    // metacharacters that would be catastrophic under word splitting.
    let tree = TempTree::new("e2e spaces");
    let main = tree.dir("main work space");
    let worktree = tree.dir("work trees/issue 42");

    fs::create_dir_all(main.join("node_modules")).unwrap();
    fs::write(main.join(".mcp.json"), "{}").unwrap();
    fs::create_dir_all(main.join("apps/my web app/node_modules")).unwrap();
    fs::write(main.join("apps/my web app/package.json"), "{}").unwrap();
    fs::create_dir_all(main.join("gen/$(touch pwned) out")).unwrap();
    fs::create_dir_all(main.join(".loom")).unwrap();
    fs::write(
        main.join(".loom/config.json"),
        r#"{"worktree":{"linkPaths":["gen/$(touch pwned) out"]}}"#,
    )
    .unwrap();

    fs::write(worktree.join("package.json"), "{}").unwrap();
    fs::create_dir_all(worktree.join("apps/my web app")).unwrap();
    let exclude_path = tree.path().join("exclude file");

    let opts = Options {
        repo_root: main.clone(),
        worktree: worktree.clone(),
        quiet: true,
    };
    let exclude = exclude_at(&exclude_path);
    link_root_node_modules(&opts, &exclude, &quiet_reporter());
    link_nested_node_modules(&opts, &exclude, &quiet_reporter());
    link_configured_paths(&opts, &exclude, &quiet_reporter());
    link_mcp_json(&opts, &exclude, &quiet_reporter());

    assert_eq!(link_target(&worktree.join("node_modules")), main.join("node_modules"));
    assert_eq!(
        link_target(&worktree.join("apps/my web app/node_modules")),
        main.join("apps/my web app/node_modules")
    );
    assert_eq!(
        link_target(&worktree.join("gen/$(touch pwned) out")),
        main.join("gen/$(touch pwned) out")
    );
    assert_eq!(link_target(&worktree.join(".mcp.json")), main.join(".mcp.json"));

    let body = fs::read_to_string(&exclude_path).unwrap();
    assert_eq!(
        body,
        "node_modules\napps/my web app/node_modules\ngen/$(touch pwned) out\n.mcp.json\n"
    );
    // Nothing was executed: word splitting is not a thing that can happen.
    assert!(!tree.path().join("pwned").exists());
    assert!(!main.join("pwned").exists());
}

#[test]
fn run_leaves_a_pre_existing_destination_alone_and_warns() {
    let tree = TempTree::new("e2e-preexisting");
    let main = tree.dir("main");
    let worktree = tree.dir("wt");
    fs::create_dir_all(main.join("node_modules")).unwrap();
    fs::write(worktree.join("package.json"), "{}").unwrap();
    // A REAL directory where the symlink would go: `! -e` is false, so the
    // shell skipped the family entirely (no attempt, no warning).
    fs::create_dir_all(worktree.join("node_modules")).unwrap();

    let exclude_path = tree.path().join("exclude");
    let opts = Options {
        repo_root: main,
        worktree: worktree.clone(),
        quiet: true,
    };
    link_root_node_modules(&opts, &exclude_at(&exclude_path), &quiet_reporter());

    assert!(!fs::symlink_metadata(worktree.join("node_modules"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!exclude_path.exists(), "no exclude entry for a skipped link");
}

#[test]
fn a_dangling_destination_symlink_is_attempted_and_warned_about() {
    // `[[ -e ]]` follows symlinks, so a DANGLING link reads as absent and the
    // shell went on to `ln -s`, which then failed (the name is taken) and
    // warned. Preserved — the alternative would be silently leaving a broken
    // link in place with an exclude entry claiming it is fine.
    let tree = TempTree::new("e2e-dangling");
    let main = tree.dir("main");
    let worktree = tree.dir("wt");
    fs::create_dir_all(main.join("node_modules")).unwrap();
    fs::write(worktree.join("package.json"), "{}").unwrap();
    symlink("/nonexistent-target", worktree.join("node_modules")).unwrap();

    let exclude_path = tree.path().join("exclude");
    let opts = Options {
        repo_root: main,
        worktree: worktree.clone(),
        quiet: true,
    };
    link_root_node_modules(&opts, &exclude_at(&exclude_path), &quiet_reporter());

    assert_eq!(
        fs::read_link(worktree.join("node_modules")).unwrap(),
        PathBuf::from("/nonexistent-target"),
        "the dangling link is untouched"
    );
    assert!(!exclude_path.exists(), "a failed link records no entry");
}

#[test]
fn run_always_reports_success() {
    // The exit-code argument in the module docs, asserted rather than only
    // stated: nothing about a worktree with nothing to link is an error.
    let tree = TempTree::new("rc");
    assert_eq!(
        run(&Options {
            repo_root: tree.dir("main"),
            worktree: tree.dir("wt"),
            quiet: true,
        }),
        0
    );
}

fn quiet_reporter() -> Reporter {
    Reporter {
        out: Out::new(false),
        quiet: true,
    }
}
