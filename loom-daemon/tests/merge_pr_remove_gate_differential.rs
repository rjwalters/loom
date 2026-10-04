//! Differential test: `loom-daemon merge-pr remove-gate` against the retired
//! shell it replaced — the #3710 primary-worktree hard guard and the
//! `.loom-managed` sentinel guard (with its `--worktree-path` bypass) at the top
//! of `merge-pr.sh`'s `_remove_loom_worktree`.
//!
//! One once-generated corpus feeds both sides (verification-recipes §6): each
//! case is a real temp directory (with or without a `.loom-managed` file), a
//! porcelain text naming it as the primary / as a linked worktree / not at
//! all, and the opt-in flag. The shell side runs the frozen block from
//! `tests/fixtures/merge-pr-remove-gate-retired.sh`; the Rust side runs the
//! real CLI. Both are reduced to the same trace: the `LEVEL<TAB>message`
//! records, then `PROCEED` iff the block fell through.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-remove-gate-retired.sh")
}

#[derive(Clone, Copy, Debug)]
enum Primary {
    /// The target IS the first `worktree ` record.
    Target,
    /// Some other checkout is first; the target is a linked worktree.
    Other,
    /// A different path that merely PREFIXES the target's.
    PrefixOfTarget,
    /// Git reported nothing.
    Empty,
    /// First record has a space in its path and is the target.
    TargetWithSpace,
}

fn porcelain_for(primary: Primary, target: &str) -> String {
    match primary {
        Primary::Target | Primary::TargetWithSpace => format!(
            "worktree {target}\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /elsewhere/wt\nHEAD bbbb\nbranch refs/heads/x\n\n"
        ),
        Primary::Other => format!(
            "worktree /repo/main\nHEAD aaaa\nbranch refs/heads/main\n\nworktree {target}\nHEAD bbbb\nbranch refs/heads/feature/issue-1\n\n"
        ),
        Primary::PrefixOfTarget => format!(
            "worktree {}\nHEAD aaaa\nbranch refs/heads/main\n\nworktree {target}\nHEAD bbbb\n\n",
            &target[..target.len() - 1]
        ),
        Primary::Empty => String::new(),
    }
}

fn run_shell(porcelain: &str, path: &str, real: &str, allow: &str) -> String {
    let driver = r#"
info() { printf 'INFO\t%s\n' "$*"; }
warning() { printf 'WARNING\t%s\n' "$*"; }
_primary_worktree_path() { printf '%s' "$PORCELAIN" | awk '/^worktree /{ print substr($0, 10); exit }'; }
source "$1"
_retired_remove_gate "$2" "$3" "$4"
"#;
    let out = Command::new("bash")
        .args(["-c", driver, "driver"])
        .arg(fixture_path())
        .args([path, real, allow])
        .env("PORCELAIN", porcelain)
        .output()
        .expect("bash ran the frozen shell side");
    assert!(
        out.status.success(),
        "frozen shell exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut trace = String::new();
    let mut proceed = false;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line == "LOOM-FELL-THROUGH" {
            proceed = true;
        } else {
            trace.push_str(line);
            trace.push('\n');
        }
    }
    trace.push_str(if proceed { "PROCEED\n" } else { "REFUSE\n" });
    trace
}

fn run_rust(porcelain: &str, path: &str, real: &str, allow: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "merge-pr",
            "remove-gate",
            "--path",
            path,
            "--real",
            real,
            "--allow-unmanaged",
            allow,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("loom-daemon spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(porcelain.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "remove-gate exited {:?}", out.status.code());
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = stdout.lines();
    let verdict = match lines.next().expect("a verdict line") {
        "LOOM-REMOVE-GATE PROCEED" => "PROCEED\n",
        "LOOM-REMOVE-GATE REFUSE" => "REFUSE\n",
        other => panic!("unrecognized verdict line {other:?}"),
    };
    let mut trace = String::new();
    for line in lines {
        trace.push_str(line);
        trace.push('\n');
    }
    trace.push_str(verdict);
    trace
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let root = tempfile::tempdir().unwrap();
    let mut checked = 0;
    for (dir_name, primary) in [
        ("plain", Primary::Target),
        ("plain", Primary::Other),
        ("plain", Primary::PrefixOfTarget),
        ("plain", Primary::Empty),
        ("a dir with spaces", Primary::TargetWithSpace),
        ("a dir with spaces", Primary::Other),
    ] {
        for sentinel in [false, true] {
            for allow in ["true", "false"] {
                let dir = root
                    .path()
                    .join(format!("{dir_name}-{primary:?}-{sentinel}-{allow}"))
                    .join(dir_name);
                std::fs::create_dir_all(&dir).unwrap();
                if sentinel {
                    std::fs::write(dir.join(".loom-managed"), "").unwrap();
                }
                let path = dir.to_str().unwrap().to_string();
                // `real` agrees with `path` here (a real directory, no
                // symlinks), which is what `pwd -P` yields; a SYMLINKED path
                // is the caller's canonicalisation and is not this verb's.
                let porcelain = porcelain_for(primary, &path);
                let shell = run_shell(&porcelain, &path, &path, allow);
                let rust = run_rust(&porcelain, &path, &path, allow);
                assert_eq!(
                    shell, rust,
                    "diverged: primary={primary:?} sentinel={sentinel} allow={allow}"
                );
                checked += 1;
            }
        }
    }
    // A sentinel that is a DIRECTORY is not `[[ -f ]]`: both must treat it as
    // absent.
    let dir = root.path().join("dir-sentinel");
    std::fs::create_dir_all(dir.join(".loom-managed")).unwrap();
    let path = dir.to_str().unwrap().to_string();
    let porcelain = porcelain_for(Primary::Other, &path);
    assert_eq!(
        run_shell(&porcelain, &path, &path, "false"),
        run_rust(&porcelain, &path, &path, "false")
    );
    // A non-canonical `real` that differs from the primary: not a match.
    let porcelain = porcelain_for(Primary::Target, &path);
    assert_eq!(
        run_shell(&porcelain, &path, "/some/other/real", "true"),
        run_rust(&porcelain, &path, "/some/other/real", "true")
    );
    assert!(checked >= 24);
}
