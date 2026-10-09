//! What `check-main-clean.sh --quarantine` asks of [`super`] (#11075), through
//! `loom-daemon stashes build-trees`.
//!
//! The script needs three answers, and each one used to be ~30 lines of
//! `contract` shell duplicating the Rust discovery: which directories are
//! cargo build trees (to exclude from the stash pathspec), which
//! `git status --porcelain` lines are nothing but build-tree content (so they
//! neither count as offending dirt nor as residual dirt after the stash), and
//! the worktree top those answers are relative to. One implementation means
//! the shell quarantine and [`super::quarantine_stash_args`] cannot drift.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{is_under_build_tree, worktree_build_tree_dirs};

/// The top level of the worktree containing `path`, or `None` when `path` is
/// not inside a git worktree. Build-tree directories are reported relative to
/// this, so a caller anchoring them with `:(top)` pathspecs gets the same
/// answer whichever subdirectory it started from.
#[must_use]
pub fn worktree_top(path: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .to_string();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// Build-tree directories of the worktree containing `path`, relative to its
/// top level. `None` when `path` is not inside a git worktree — a caller must
/// treat that as "could not check", never as "no build trees".
#[must_use]
pub fn build_tree_dirs(path: &Path) -> Option<BTreeSet<String>> {
    worktree_top(path).map(|top| worktree_build_tree_dirs(&top))
}

/// `status` (`git status --porcelain` v1 text for the worktree at `top`)
/// without the lines that are build-tree content, every other line passed
/// through unchanged and in order.
///
/// A line is dropped when its path (a rename's destination) is, or lies
/// under, one of `dirs`; or when it is a collapsed untracked directory
/// (`?? .loom/`) whose every untracked file lies under one. A collapsed
/// directory that also holds real work is KEPT — its build trees are excluded
/// by pathspec, not by dropping the line — and so is any line whose check
/// cannot be made: dropping real dirt from a rescue is the worse failure.
#[must_use]
pub fn filter_status(top: &Path, dirs: &BTreeSet<String>, status: &str) -> String {
    if dirs.is_empty() {
        return status.to_string();
    }
    status
        .lines()
        .filter(|line| !line.trim().is_empty() && !is_build_tree_line(top, dirs, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_build_tree_line(top: &Path, dirs: &BTreeSet<String>, line: &str) -> bool {
    let Some(raw) = line.get(3..) else {
        return false;
    };
    let raw = raw.rsplit_once(" -> ").map_or(raw, |(_, to)| to);
    let path = git_unquote(raw);
    if is_under_build_tree(&path, dirs) {
        return true;
    }
    line.starts_with("??") && path.ends_with('/') && only_build_tree_content(top, dirs, &path)
}

/// Whether every untracked file under the collapsed directory `dir` lies in a
/// build tree. `false` whenever that cannot be established.
fn only_build_tree_content(top: &Path, dirs: &BTreeSet<String>, dir: &str) -> bool {
    let inner: Vec<&String> = dirs.iter().filter(|d| d.starts_with(dir)).collect();
    if inner.is_empty() {
        return false;
    }
    let mut args = vec![
        "ls-files".to_string(),
        "--others".to_string(),
        "--exclude-standard".to_string(),
        "--".to_string(),
        format!(":(literal,top){dir}"),
    ];
    args.extend(inner.iter().map(|d| format!(":(exclude,literal,top){d}")));
    Command::new("git")
        .args(&args)
        .current_dir(top)
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.is_empty())
}

/// Undo git's C-style path quoting (`"a\tb"`, `"caf\303\251"`); an unquoted
/// path is returned as-is.
fn git_unquote(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return raw.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(d @ b'0'..=b'7') => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(&n @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(n - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(v & 0xff).unwrap_or(b'?'));
            }
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
        dir
    }

    #[test]
    fn unquote_handles_escapes_and_octal_utf8() {
        assert_eq!(git_unquote("plain/path"), "plain/path");
        assert_eq!(git_unquote(r#""a\tb""#), "a\tb");
        assert_eq!(git_unquote(r#""caf\303\251/x""#), "café/x");
        assert_eq!(git_unquote(r#""q\"uote\\d""#), "q\"uote\\d");
    }

    #[test]
    fn dirs_are_relative_to_the_top_from_any_subdirectory() {
        let r = repo();
        let root = r.path();
        write(root, "sub/target-x/CACHEDIR.TAG", SIG);
        write(root, "sub/target-x/a.o", "bin");
        let want: BTreeSet<String> = ["sub/target-x".to_string()].into();
        assert_eq!(build_tree_dirs(root), Some(want.clone()));
        assert_eq!(build_tree_dirs(&root.join("sub")), Some(want));
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(build_tree_dirs(outside.path()), None, "not a worktree is not 'no trees'");
    }

    #[test]
    fn filter_drops_build_tree_lines_and_keeps_real_dirt() {
        let r = repo();
        let root = r.path();
        write(root, ".loom/target-doc/CACHEDIR.TAG", SIG);
        write(root, ".loom/target-doc/a.o", "bin");
        write(root, "target-x/CACHEDIR.TAG", SIG);
        write(root, "mixed/target-y/CACHEDIR.TAG", SIG);
        write(root, "mixed/real.txt", "keep");
        let dirs = build_tree_dirs(root).unwrap();
        let status = "?? .loom/\n?? target-x/\n?? mixed/\n A target-x/a.o\nR  old.txt -> target-x/b.o\n M src/lib.rs\n?? \"caf\\303\\251.txt\"\n?? target/\n";
        assert_eq!(
            filter_status(root, &dirs, status),
            "?? mixed/\n M src/lib.rs\n?? \"caf\\303\\251.txt\"\n?? target/"
        );
        // No build trees: byte-for-byte passthrough.
        assert_eq!(filter_status(root, &BTreeSet::new(), status), status);
    }
}
