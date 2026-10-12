//! Differential test: `loom-daemon merge-pr discovered-worktree` against the
//! retired shell it replaced — the primary / managed / user-owned
//! classification in `merge-pr.sh`'s porcelain discovery fallback.
//!
//! One once-generated corpus feeds both sides (verification-recipes §6): real
//! temp directories with or without a `.loom-managed` file (or a directory of
//! that name), crossed with the primary flag and branch/path names containing
//! spaces and quotes. Both sides reduce to the same trace: the
//! `LEVEL<TAB>message` records, then `DECIDE` iff the shared remove-vs-preserve
//! decision would have been invoked.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-discovered-worktree-retired.sh")
}

fn run_shell(path: &str, branch: &str, primary: bool) -> String {
    let driver = r#"
info() { printf 'INFO\t%s\n' "$*"; }
warning() { printf 'WARNING\t%s\n' "$*"; }
_is_primary_worktree_path() { [[ "$PRIMARY" == "true" ]]; }
_worktree_cleanup_decide() { printf 'DECIDE\n'; }
source "$1"
_retired_discovered_worktree "$2" "$3"
"#;
    let out = Command::new("bash")
        .args(["-c", driver, "driver"])
        .arg(fixture_path())
        .args([path, branch])
        .env("PRIMARY", if primary { "true" } else { "false" })
        .output()
        .expect("bash ran the frozen shell side");
    assert!(out.status.success(), "frozen shell failed");
    let mut trace = String::new();
    let mut decide = false;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line == "DECIDE" {
            decide = true;
        } else {
            trace.push_str(line);
            trace.push('\n');
        }
    }
    if decide {
        trace.push_str("DECIDE\n");
    }
    trace
}

fn run_rust(path: &str, branch: &str, primary: bool) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "merge-pr",
            "discovered-worktree",
            "--branch",
            branch,
            "--path",
            path,
            "--primary",
            if primary { "true" } else { "false" },
        ])
        .output()
        .expect("loom-daemon spawned");
    assert!(out.status.success(), "discovered-worktree exited {:?}", out.status.code());
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = stdout.lines();
    let decide = match lines.next().expect("a verdict line") {
        "LOOM-DISCOVERED DECIDE" => true,
        "LOOM-DISCOVERED NOTE" => false,
        other => panic!("unrecognized verdict line {other:?}"),
    };
    let mut trace = String::new();
    for line in lines {
        trace.push_str(line);
        trace.push('\n');
    }
    if decide {
        trace.push_str("DECIDE\n");
    }
    trace
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let root = tempfile::tempdir().unwrap();
    let mut checked = 0;
    for dir_name in ["plain", "a dir with spaces", "it's quoted"] {
        for branch in ["feature/issue-9", "docs/a b", "weird'branch"] {
            // sentinel: none / regular file / directory
            for sentinel in ["none", "file", "dir"] {
                for primary in [false, true] {
                    let dir = root.path().join(format!("{checked}")).join(dir_name);
                    std::fs::create_dir_all(&dir).unwrap();
                    match sentinel {
                        "file" => std::fs::write(dir.join(".loom-managed"), "").unwrap(),
                        "dir" => std::fs::create_dir_all(dir.join(".loom-managed")).unwrap(),
                        _ => {}
                    }
                    let path = dir.to_str().unwrap();
                    assert_eq!(
                        run_shell(path, branch, primary),
                        run_rust(path, branch, primary),
                        "diverged: dir={dir_name} branch={branch} sentinel={sentinel} primary={primary}"
                    );
                    checked += 1;
                }
            }
        }
    }
    // A path that does not exist at all is simply "no sentinel".
    assert_eq!(
        run_shell("/nonexistent/wt", "b", false),
        run_rust("/nonexistent/wt", "b", false)
    );
    assert!(checked >= 50);
}
