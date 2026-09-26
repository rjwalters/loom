//! Unit tests for the submodule-initialization port (#8195 slice 8).
//!
//! The differential harness (`tests/worktree_submodules_differential.rs`) is
//! the equivalence evidence and it needs real clones, so it is slow and it can
//! only reach the shapes git will actually produce. These are the cases it
//! cannot reach cheaply: the `git submodule status` grammar one line at a
//! time (including the space-bearing and non-UTF-8 paths a shell fixture
//! cannot express without becoming a test of the fixture), and the
//! reference-path resolution that the retired shell got wrong in a way no
//! black-box output could show.

use super::*;

// ---------------------------------------------------------------------------
// parse_uninitialized — the #7858 class
// ---------------------------------------------------------------------------

fn parsed(stdout: &str) -> Vec<String> {
    parse_uninitialized(stdout.as_bytes())
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

const OID: &str = "0000000000000000000000000000000000000001";

#[test]
fn empty_output_is_no_work() {
    assert!(parsed("").is_empty());
    assert!(parsed("\n").is_empty());
}

#[test]
fn only_dash_lines_are_uninitialized() {
    // ' ' = in sync, '+' = checked-out commit differs, 'U' = merge conflict.
    // The retired `grep '^-'` took only the first kind and so does this; the
    // other three are already populated and re-running `update --init` on
    // them would be a behaviour change, not a fix.
    let out = format!(
        " {OID} in-sync (heads/main)\n\
         +{OID} moved (heads/main)\n\
         U{OID} conflicted\n\
         -{OID} fresh\n"
    );
    assert_eq!(parsed(&out), vec!["fresh".to_string()]);
}

/// The defect this slice exists for. `awk '{print $2}'` yields `vendor` here
/// and git is then handed `vendor` as both the `--reference` directory
/// component and the pathspec — neither of which names anything.
#[test]
fn a_path_containing_spaces_survives_whole() {
    let out = format!("-{OID} vendor/my lib/deep nest\n");
    assert_eq!(parsed(&out), vec!["vendor/my lib/deep nest".to_string()]);
}

/// Consecutive spaces would collapse under any field-splitting parse; here
/// they are part of the path and must be preserved byte-exactly.
#[test]
fn consecutive_spaces_in_a_path_are_preserved() {
    let out = format!("-{OID} a  b\n");
    assert_eq!(parsed(&out), vec!["a  b".to_string()]);
}

/// `-` lines carry no ` (describe)` suffix — that is what makes the
/// "everything after the oid is the path" rule sound. A path that happens to
/// END in something describe-shaped must therefore NOT be trimmed.
#[test]
fn a_parenthesised_tail_on_a_dash_line_is_part_of_the_path() {
    let out = format!("-{OID} vendor/lib (old)\n");
    assert_eq!(parsed(&out), vec!["vendor/lib (old)".to_string()]);
}

#[test]
fn a_non_utf8_path_is_carried_through_as_bytes() {
    let mut line = Vec::new();
    line.push(b'-');
    line.extend_from_slice(OID.as_bytes());
    line.push(b' ');
    line.extend_from_slice(b"vendor/\xff\xfelib");
    line.push(b'\n');

    let got = parse_uninitialized(&line);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].as_bytes(), b"vendor/\xff\xfelib");
}

#[test]
fn a_dash_line_with_no_path_is_skipped() {
    // Not something git emits; the point is that it yields nothing rather
    // than the empty pathspec `awk` would have produced.
    let out = format!("-{OID} \n-{OID}\n");
    assert!(parsed(&out).is_empty());
}

#[test]
fn several_entries_keep_their_order() {
    let out = format!("-{OID} a\n-{OID} b\n-{OID} c\n");
    assert_eq!(parsed(&out), vec!["a", "b", "c"]);
}

#[test]
fn a_missing_trailing_newline_still_yields_the_last_entry() {
    let out = format!("-{OID} a\n-{OID} b");
    assert_eq!(parsed(&out), vec!["a", "b"]);
}

// ---------------------------------------------------------------------------
// reference_dir — the concatenation, not a join
// ---------------------------------------------------------------------------

#[test]
fn reference_dir_appends_under_modules() {
    let got = reference_dir(Path::new("/repo/.git"), OsStr::new("vendor/lib"));
    assert_eq!(got, PathBuf::from("/repo/.git/modules/vendor/lib"));
}

#[test]
fn reference_dir_keeps_spaces() {
    let got = reference_dir(Path::new("/re po/.git"), OsStr::new("my lib"));
    assert_eq!(got, PathBuf::from("/re po/.git/modules/my lib"));
}

/// `Path::join` would return `/etc/ssh` and point `--reference` at something
/// outside the workspace. The shell's `"$MAIN_GIT_DIR/modules/$submod_path"`
/// could not do that, so neither does this.
#[test]
fn reference_dir_does_not_let_an_absolute_component_escape() {
    let got = reference_dir(Path::new("/repo/.git"), OsStr::new("/etc/ssh"));
    assert_eq!(got, PathBuf::from("/repo/.git/modules//etc/ssh"));
    assert!(got.starts_with("/repo/.git/modules"));
}

// ---------------------------------------------------------------------------
// git_common_dir — defect 3, the fast path that never fired
// ---------------------------------------------------------------------------

/// A throwaway directory that removes itself.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("loom-wt-submod-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp tree");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        status.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
}

/// The regression for defect 3. `git rev-parse --git-common-dir` answers
/// `.git` at a repo root; the retired shell kept that relative string and
/// tested it from inside the worktree, where `.git` is a FILE. Anchoring it
/// to the repo root is what makes `--reference` reachable — and the assertion
/// that fails if the anchoring is removed is `is_absolute`.
#[test]
fn git_common_dir_is_absolute_even_though_git_answers_relatively() {
    let tree = TempTree::new("common");
    let repo = tree.path();
    init_repo(repo);

    let raw = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .expect("git runs");
    let raw = String::from_utf8_lossy(&raw.stdout).trim().to_string();
    assert!(
        !Path::new(&raw).is_absolute(),
        "precondition: git is expected to answer relatively at a repo root, got {raw:?}"
    );

    let resolved = git_common_dir(repo).expect("resolved");
    assert!(resolved.is_absolute(), "got {resolved:?}");
    assert!(resolved.exists(), "{resolved:?} should exist");
    assert!(is_dir(&resolved));
}

#[test]
fn git_common_dir_is_none_outside_a_repo() {
    let tree = TempTree::new("norepo");
    // A bare directory with no repo above it in the temp root. If the temp
    // root itself were inside a repo this would resolve, so assert the
    // precondition rather than the conclusion.
    let answered = Command::new("git")
        .current_dir(tree.path())
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .expect("git runs");
    if answered.status.success() {
        return; // temp dir lives inside a repo on this host; nothing to test
    }
    assert!(git_common_dir(tree.path()).is_none());
}
