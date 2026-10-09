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
/// A line is dropped when its path is, or lies under, one of `dirs`; or when
/// it is a collapsed untracked directory (`?? .loom/`) whose every untracked
/// file lies under one. A collapsed directory that also holds real work is
/// KEPT — its build trees are excluded by pathspec, not by dropping the line —
/// and so is any line whose check cannot be made: dropping real dirt from a
/// rescue is the worse failure.
///
/// A rename (`R`) INTO a build tree is split (#11149): its destination is
/// build-tree content, but its source's deletion is real dirt, so the line
/// becomes `D  <source>` (the source's quoting preserved) and is dropped only
/// when the source lies in a build tree too. A copy (`C`) leaves its source
/// untouched, so one into a build tree is dropped whole. Only an `R`/`C` line
/// is split on ` -> ` at all: any other line's path is the whole field.
#[must_use]
pub fn filter_status(top: &Path, dirs: &BTreeSet<String>, status: &str) -> String {
    if dirs.is_empty() {
        return status.to_string();
    }
    status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| filter_line(top, dirs, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One porcelain line through the build-tree filter: `None` to drop it,
/// otherwise the line to keep (rewritten only for a rename into a build tree).
fn filter_line(top: &Path, dirs: &BTreeSet<String>, line: &str) -> Option<String> {
    let (Some(xy), Some(raw)) = (line.get(..2), line.get(3..)) else {
        return Some(line.to_string());
    };
    let split = if xy.contains(['R', 'C']) {
        split_rename(raw)
    } else {
        None
    };
    let Some((from, to)) = split else {
        let path = git_unquote(raw);
        let drop = is_under_build_tree(&path, dirs)
            || (xy == "??" && path.ends_with('/') && only_build_tree_content(top, dirs, &path));
        return (!drop).then(|| line.to_string());
    };
    if !is_under_build_tree(&git_unquote(to), dirs) {
        return Some(line.to_string());
    }
    let keep_source = xy.contains('R') && !is_under_build_tree(&git_unquote(from), dirs);
    keep_source.then(|| format!("D  {from}"))
}

/// Split a rename/copy field `<from> -> <to>` into its raw (still quoted)
/// sides. A quoted source is scanned to its closing quote, so a ` -> ` inside
/// it is not taken as the separator; an unquoted source cannot contain a
/// space, so its first ` -> ` is the separator.
fn split_rename(raw: &str) -> Option<(&str, &str)> {
    if !raw.starts_with('"') {
        return raw.split_once(" -> ");
    }
    let bytes = raw.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => {
                let (from, rest) = raw.split_at(i + 1);
                return rest.strip_prefix(" -> ").map(|to| (from, to));
            }
            _ => i += 1,
        }
    }
    None
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
        // The rename's destination is build-tree content; its source's
        // deletion is not, and survives as a `D` line (#11149).
        assert_eq!(
            filter_status(root, &dirs, status),
            "?? mixed/\nD  old.txt\n M src/lib.rs\n?? \"caf\\303\\251.txt\"\n?? target/"
        );
        // No build trees: byte-for-byte passthrough.
        assert_eq!(filter_status(root, &BTreeSet::new(), status), status);
    }

    #[test]
    fn rename_into_a_build_tree_keeps_only_the_source_deletion() {
        let r = repo();
        let root = r.path();
        write(root, "target-x/CACHEDIR.TAG", SIG);
        write(root, "target-y/CACHEDIR.TAG", SIG);
        let dirs = build_tree_dirs(root).unwrap();
        let f = |s: &str| filter_status(root, &dirs, s);
        // Both sides in build trees: nothing real left.
        assert_eq!(f("R  target-y/a.o -> target-x/b.o\n"), "");
        // A quoted source stays quoted, ` -> ` inside it included.
        assert_eq!(f("RM \"a -> b.txt\" -> target-x/b.o\n"), "D  \"a -> b.txt\"");
        assert_eq!(f("R  \"caf\\303\\251\" -> \"target-x/x y\"\n"), "D  \"caf\\303\\251\"");
        // A copy leaves its source alone: dropped whole.
        assert_eq!(f("C  src.txt -> target-x/c.o\n"), "");
        // Out of a build tree into real work: kept unchanged.
        assert_eq!(f("R  target-x/a.o -> real.txt\n"), "R  target-x/a.o -> real.txt");
    }

    #[test]
    fn a_literal_arrow_in_a_non_rename_path_is_not_a_separator() {
        let r = repo();
        let root = r.path();
        write(root, "b\"/CACHEDIR.TAG", SIG);
        write(root, "target-x/CACHEDIR.TAG", SIG);
        let dirs = build_tree_dirs(root).unwrap();
        // The whole path `a -> b` is checked, not the bogus tail `b"`.
        assert_eq!(filter_status(root, &dirs, "?? \"a -> b\"\n"), "?? \"a -> b\"");
        assert_eq!(
            filter_status(root, &dirs, "?? \"target-x/a -> b\"\n"),
            "",
            "a build-tree path containing ` -> ` is still recognised whole"
        );
    }
}
