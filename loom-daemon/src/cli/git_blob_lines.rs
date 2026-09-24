//! `loom-daemon git-blob-lines <path>` — resolve a path against a git rev,
//! following any `120000` (symlink) tree entry to its real target blob, and
//! report the resolved blob's line count (#8656).
//!
//! # Why this exists
//!
//! `defaults/scripts/verify-proposal-refs.sh`'s line-range check used to read
//! a cited path straight through `git show "origin/main:$path" | wc -l`.
//! Since #7842 every `.loom/docs/*.md` file with a `defaults/docs/`
//! counterpart is a **symlink** (tree mode `120000`) on `origin/main`, so that
//! read returned the link-target STRING, not the target file's content —
//! `wc -l` measured the link (0 or 1 "lines") and every genuinely in-range
//! citation under `.loom/docs/` was reported as a miss. This mirrors the
//! symlink-blindness Champion's premise-false gate had (#8593, fixed by
//! `resolve_main_path()` in
//! `defaults/.claude/commands/loom/champion-premise-false-evidence.md`) — the
//! resolution algorithm here is the same one, ported to Rust so the `contract`
//! -category shell script (`scripts/shell-allowlist.txt`) does not have to
//! grow to carry it (`.loom/docs/shell-language-policy.md`).
//!
//! # Resolution semantics
//!
//! - Bounded to [`MAX_HOPS`] hops, so a symlink cycle terminates instead of
//!   looping — mirroring `resolve_main_path()`'s own 3-hop bound.
//! - A symlink target is resolved **relative to the link's own directory**
//!   (git's symlink semantics), not the caller's cwd or the original path's
//!   directory.
//! - Normalization of `.`/`..` segments is purely **lexical**. This path
//!   lives inside a commit tree, not the working directory, so
//!   `std::fs::canonicalize` (which stats the filesystem) would be the wrong
//!   tool even when the same path happens to exist on disk in the current
//!   checkout.
//!
//! # Exit-code contract
//!
//! ## Report mode (no `--range`)
//!
//! | Exit | Meaning | stdout |
//! |---|---|---|
//! | `0` | Resolved to a regular blob | `<resolved-path>\t<line-count>` |
//! | [`EX_UNREADABLE`] (10) | Dangling symlink, unresolved cycle, or a non-blob entry — **inconclusive**, never reported as "0 lines" | `<resolved-path>\t` |
//! | [`EX_USAGE`] (2) | `git` could not be run, the rev does not resolve, or another usage error | (none) |
//!
//! ## Range mode (`--range L` / `--range L1-L2`)
//!
//! The caller is asking a yes/no question about a cited span, so the answer is
//! the exit code and the whole diagnostic is rendered here — that is the point
//! of the port (`verify-proposal-refs.sh` is `contract`-category shell whose
//! portable line count may not grow, `.loom/docs/shell-language-policy.md`).
//!
//! | Exit | Meaning | Output |
//! |---|---|---|
//! | `0` | The span fits inside the resolved blob | (none) |
//! | [`EX_OUT_OF_RANGE`] (1) | The span runs past the end of the resolved blob | stdout: `BAD LINE RANGE: …` — the caller appends it to its own miss list |
//! | [`EX_UNREADABLE`] (10) | Unreadable target — the span could NOT be checked | stderr: `BROKEN SYMLINK: …`. **Not a miss**: an unreadable file is an inconclusive check, not a disproof (#8593's distinction, applied here) |
//! | [`EX_USAGE`] (2) | Malformed `--range`, or the git-level failures above | stderr |

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Usage / precondition failure: `git` missing/failed, or the rev/workspace
/// does not resolve. Distinct from [`EX_UNREADABLE`] — this means the
/// question could not be asked at all, not that it was asked and the target
/// turned out to be unreadable.
pub(crate) const EX_USAGE: i32 = 2;

/// The resolved path is a dangling symlink target, an unresolved link cycle
/// (still `120000` after [`MAX_HOPS`] hops), or a tree entry that is not a
/// regular blob (e.g. a subdirectory). This is an **inconclusive** check, not
/// a disproof — the caller must not treat it as "0 lines".
pub(crate) const EX_UNREADABLE: i32 = 10;

/// Range mode only: the cited span runs past the end of the resolved blob.
/// This IS a disproof — the caller records it as a miss.
pub(crate) const EX_OUT_OF_RANGE: i32 = 1;

/// Bounded hop count. Mirrors `resolve_main_path()` in
/// `defaults/.claude/commands/loom/champion-premise-false-evidence.md` — kept
/// numerically identical to that established convention on purpose, rather
/// than picking an independent bound for the same resolution.
const MAX_HOPS: u32 = 3;

/// One resolution outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// Resolved to a regular (`100644`/`100755`) blob with this many lines
    /// (`wc -l` semantics: the count of `\n` bytes in the content).
    Lines { resolved_path: String, count: u64 },
    /// Dangling symlink, unresolved cycle, or non-blob entry.
    Unreadable { resolved_path: String },
}

fn git(workspace: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .with_context(|| format!("could not run git {args:?} in {}", workspace.display()))
}

/// `git ls-tree <rev> -- <path>` mode column, or `None` when the path does
/// not exist in the tree at that rev at all.
fn tree_entry_mode(workspace: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    let out = git(workspace, &["ls-tree", rev, "--", path])?;
    if !out.status.success() {
        anyhow::bail!(
            "git ls-tree {rev} -- {path} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string))
}

/// Raw content of the blob at `rev:path` — used both to read a symlink's
/// target string and to count a regular blob's lines.
fn read_blob(workspace: &Path, rev: &str, path: &str) -> Result<Vec<u8>> {
    let out = git(workspace, &["show", &format!("{rev}:{path}")])?;
    if !out.status.success() {
        anyhow::bail!(
            "git show {rev}:{path} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Collapse `.` / `..` segments **lexically**. See the module doc for why
/// this must not touch the filesystem.
fn normalize_lexically(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

/// The directory component of a repo-relative path, or `""` for a top-level
/// path.
fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Follow `path` through up to [`MAX_HOPS`] `120000` tree entries at `rev`,
/// resolving each target relative to the **link's own directory**. Returns
/// once the current path stops being a symlink tree entry — which may still
/// not exist (a dangling target) or may still be `120000` if the hop bound
/// was hit (an unresolved cycle); the caller classifies that via a final
/// [`tree_entry_mode`] check.
fn resolve_symlink_path(workspace: &Path, rev: &str, path: &str) -> Result<String> {
    let mut current = normalize_lexically(path);
    for _ in 0..MAX_HOPS {
        let mode = tree_entry_mode(workspace, rev, &current)?;
        if mode.as_deref() != Some("120000") {
            break;
        }
        let target_bytes = read_blob(workspace, rev, &current)?;
        let target = String::from_utf8_lossy(&target_bytes);
        let target = target.trim_end_matches('\n');
        let dir = parent_dir(&current);
        let joined = if dir.is_empty() {
            target.to_string()
        } else {
            format!("{dir}/{target}")
        };
        current = normalize_lexically(&joined);
    }
    Ok(current)
}

/// Resolve `path` against `rev` in `workspace`, then classify the result.
///
/// # Errors
/// When `git` cannot be run, or a `git` invocation itself fails (e.g. `rev`
/// does not resolve).
pub(crate) fn resolve(workspace: &Path, rev: &str, path: &str) -> Result<Resolution> {
    let resolved = resolve_symlink_path(workspace, rev, path)?;
    let mode = tree_entry_mode(workspace, rev, &resolved)?;
    match mode.as_deref() {
        Some("100644") | Some("100755") => {
            let content = read_blob(workspace, rev, &resolved)?;
            let count = content.iter().filter(|&&b| b == b'\n').count() as u64;
            Ok(Resolution::Lines {
                resolved_path: resolved,
                count,
            })
        }
        _ => Ok(Resolution::Unreadable {
            resolved_path: resolved,
        }),
    }
}

/// Parse a `--range` spec: `L` (a single line) or `L1-L2` (a span). Returns
/// `(start, end)`; a single line is `(L, L)`.
///
/// # Errors
/// When the spec is not one or two decimal numbers separated by `-`.
pub(crate) fn parse_range(spec: &str) -> Result<(u64, u64)> {
    let (start_s, end_s) = spec.split_once('-').unwrap_or((spec, spec));
    let parse = |s: &str| -> Result<u64> {
        s.parse::<u64>()
            .with_context(|| format!("not a line number: {s:?} (in --range {spec:?})"))
    };
    Ok((parse(start_s)?, parse(end_s)?))
}

/// Range mode's verdict, kept separate from the printing so tests can assert
/// the decision without capturing process output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RangeVerdict {
    /// The cited span fits inside the resolved blob.
    InRange,
    /// The span runs past the end of the resolved blob — a real miss.
    OutOfRange { resolved_path: String, count: u64 },
    /// The target could not be read, so the span was never checked.
    Unreadable { resolved_path: String },
}

/// Check a cited line span against the resolved blob at `rev`.
///
/// # Errors
/// Propagates [`resolve`]'s git-level failures.
pub(crate) fn check_range(
    workspace: &Path,
    rev: &str,
    path: &str,
    start: u64,
    end: u64,
) -> Result<RangeVerdict> {
    match resolve(workspace, rev, path)? {
        Resolution::Lines {
            resolved_path,
            count,
        } => {
            if start > count || end > count {
                Ok(RangeVerdict::OutOfRange {
                    resolved_path,
                    count,
                })
            } else {
                Ok(RangeVerdict::InRange)
            }
        }
        Resolution::Unreadable { resolved_path } => Ok(RangeVerdict::Unreadable { resolved_path }),
    }
}

/// `loom-daemon git-blob-lines <PATH> [--rev REV] [--workspace DIR]
/// [--range SPEC] [--cite TEXT]`.
#[derive(clap::Args)]
pub(crate) struct GitBlobLinesArgs {
    /// Path to resolve, relative to the repo root.
    #[arg(value_name = "PATH")]
    path: String,

    /// Git rev to resolve the path against.
    #[arg(long, default_value = "origin/main")]
    rev: String,

    /// Repo/workspace root to run `git` in (default: current directory).
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// Range mode: check the cited span `L` or `L1-L2` against the RESOLVED
    /// blob instead of reporting its line count. Exits 0 in range; 1 with a
    /// `BAD LINE RANGE: …` line on stdout when the span runs past the end; 10
    /// with a `BROKEN SYMLINK: …` line on stderr when the target is unreadable
    /// and the span therefore could not be checked at all (inconclusive — the
    /// caller must NOT record a miss).
    #[arg(long, value_name = "SPEC")]
    range: Option<String>,

    /// Range mode: the citation text to quote in the diagnostic, as the
    /// caller's source actually wrote it (default: `<PATH>:<SPEC>`).
    #[arg(long, value_name = "TEXT")]
    cite: Option<String>,
}

impl GitBlobLinesArgs {
    /// Never returns: exits with the code documented on the module.
    pub(crate) fn run(self) -> Result<()> {
        handle(
            &self.path,
            &self.rev,
            self.workspace,
            self.range.as_deref(),
            self.cite.as_deref(),
        )
    }
}

/// Handle `loom-daemon git-blob-lines <path> [--rev REV] [--workspace DIR]
/// [--range SPEC] [--cite TEXT]`. Never returns (exits the process with the
/// code documented on the module).
fn handle(
    path: &str,
    rev: &str,
    workspace: Option<PathBuf>,
    range: Option<&str>,
    cite: Option<&str>,
) -> Result<()> {
    let workspace = match workspace {
        Some(w) => w,
        None => std::env::current_dir().context(
            "loom-daemon git-blob-lines: could not resolve the current directory; pass --workspace",
        )?,
    };

    if let Some(spec) = range {
        let (start, end) = match parse_range(spec) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("loom-daemon git-blob-lines: {e:#}");
                std::process::exit(EX_USAGE);
            }
        };
        // The citation as the caller's reader wrote it, so the diagnostic
        // quotes what is actually in the proposal body rather than a path the
        // author never typed.
        let cite = cite.map_or_else(|| format!("{path}:{spec}"), str::to_string);
        match check_range(&workspace, rev, path, start, end) {
            Err(e) => {
                eprintln!("loom-daemon git-blob-lines: {e:#}");
                std::process::exit(EX_USAGE);
            }
            Ok(RangeVerdict::InRange) => std::process::exit(0),
            Ok(RangeVerdict::OutOfRange {
                resolved_path,
                count,
            }) => {
                // stdout, because the caller splices this straight into its
                // own miss list. Names the RESOLVED path: when the cited path
                // is a symlink, `<cite>` and `<resolved_path>` differ, and the
                // reader needs to see which document the count came from.
                println!("BAD LINE RANGE: `{cite}` — {rev}:{resolved_path} has only {count} lines");
                std::process::exit(EX_OUT_OF_RANGE);
            }
            Ok(RangeVerdict::Unreadable { resolved_path }) => {
                // stderr, because this is NOT a miss — the check could not be
                // performed at all, and an inconclusive check must never block
                // filing the way a disproof does.
                eprintln!(
                    "  ! BROKEN SYMLINK: `{cite}` — {rev}:{resolved_path} is unreadable \
                     (dangling symlink, unresolved link cycle, or not a regular file); the line \
                     range was NOT checked — inconclusive, not a miss"
                );
                std::process::exit(EX_UNREADABLE);
            }
        }
    }

    let resolution = match resolve(&workspace, rev, path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("loom-daemon git-blob-lines: {e:#}");
            std::process::exit(EX_USAGE);
        }
    };

    match resolution {
        Resolution::Lines {
            resolved_path,
            count,
        } => {
            println!("{resolved_path}\t{count}");
            std::process::exit(0);
        }
        Resolution::Unreadable { resolved_path } => {
            // Print the resolved path on stdout too (tab, empty count) so a
            // caller can name it in a message on the failing branch without a
            // second invocation — the non-zero exit is what signals
            // "inconclusive", not the absence of stdout.
            println!("{resolved_path}\t");
            eprintln!(
                "loom-daemon git-blob-lines: {rev}:{resolved_path} is unreadable (dangling \
                 symlink, unresolved link cycle, or not a regular file) — inconclusive, not \
                 \"0 lines\""
            );
            std::process::exit(EX_UNREADABLE);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Build a git repo fixture with one commit containing:
    /// - `regular.md` (a real file with a known line count)
    /// - `docs/link.md` -> `../regular.md` (a symlink, relative to the
    ///   link's own directory, exercising the cross-directory relative case)
    /// - `docs/dangling.md` -> `../missing.md` (a symlink to nothing)
    /// - `a.md` -> `b.md`, `b.md` -> `a.md` (a 2-hop cycle)
    fn build_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());

        fs::write(root.join("regular.md"), "one\ntwo\nthree\n").unwrap();
        fs::create_dir(root.join("docs")).unwrap();
        symlink("../regular.md", root.join("docs/link.md")).unwrap();
        symlink("../missing.md", root.join("docs/dangling.md")).unwrap();
        symlink("b.md", root.join("a.md")).unwrap();
        symlink("a.md", root.join("b.md")).unwrap();

        assert!(Command::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-q", "-m", "fixture"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());

        dir
    }

    #[test]
    fn regular_file_reports_its_own_line_count() {
        let dir = build_fixture();
        let r = resolve(dir.path(), "HEAD", "regular.md").unwrap();
        assert_eq!(
            r,
            Resolution::Lines {
                resolved_path: "regular.md".to_string(),
                count: 3
            }
        );
    }

    #[test]
    fn symlink_resolves_relative_to_its_own_directory() {
        let dir = build_fixture();
        // docs/link.md -> ../regular.md, i.e. repo-root regular.md.
        let r = resolve(dir.path(), "HEAD", "docs/link.md").unwrap();
        assert_eq!(
            r,
            Resolution::Lines {
                resolved_path: "regular.md".to_string(),
                count: 3
            }
        );
    }

    #[test]
    fn dangling_symlink_is_unreadable_not_zero_lines() {
        let dir = build_fixture();
        let r = resolve(dir.path(), "HEAD", "docs/dangling.md").unwrap();
        match r {
            Resolution::Unreadable { resolved_path } => {
                assert_eq!(resolved_path, "missing.md");
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn symlink_cycle_is_unreadable_after_bounded_hops() {
        let dir = build_fixture();
        let r = resolve(dir.path(), "HEAD", "a.md").unwrap();
        // After MAX_HOPS the entry is still a 120000 tree entry -- neither
        // side of the cycle is ever a regular blob.
        assert!(matches!(r, Resolution::Unreadable { .. }), "{r:?}");
    }

    #[test]
    fn missing_path_is_unreadable() {
        let dir = build_fixture();
        let r = resolve(dir.path(), "HEAD", "does/not/exist.md").unwrap();
        assert!(matches!(r, Resolution::Unreadable { .. }), "{r:?}");
    }

    #[test]
    fn bad_rev_is_an_error_not_a_resolution() {
        let dir = build_fixture();
        let err = resolve(dir.path(), "not-a-real-rev", "regular.md").unwrap_err();
        assert!(err.to_string().contains("ls-tree"), "{err}");
    }

    #[test]
    fn range_inside_a_symlinked_document_is_in_range() {
        let dir = build_fixture();
        // The whole point of #8656: the link measures 1 line, the document 3.
        let v = check_range(dir.path(), "HEAD", "docs/link.md", 2, 3).unwrap();
        assert_eq!(v, RangeVerdict::InRange);
    }

    #[test]
    fn out_of_range_names_the_resolved_path_and_the_real_count() {
        let dir = build_fixture();
        let v = check_range(dir.path(), "HEAD", "docs/link.md", 1, 99).unwrap();
        assert_eq!(
            v,
            RangeVerdict::OutOfRange {
                resolved_path: "regular.md".to_string(),
                count: 3
            }
        );
    }

    #[test]
    fn range_against_a_dangling_symlink_is_unreadable_not_out_of_range() {
        let dir = build_fixture();
        let v = check_range(dir.path(), "HEAD", "docs/dangling.md", 1, 1).unwrap();
        assert_eq!(
            v,
            RangeVerdict::Unreadable {
                resolved_path: "missing.md".to_string()
            },
            "an unreadable target is an inconclusive check, never a 0-line disproof"
        );
    }

    #[test]
    fn parse_range_accepts_a_single_line_and_a_span() {
        assert_eq!(parse_range("42").unwrap(), (42, 42));
        assert_eq!(parse_range("100-200").unwrap(), (100, 200));
        assert!(parse_range("").is_err());
        assert!(parse_range("10-x").is_err());
    }

    #[test]
    fn lexical_normalize_collapses_dot_and_dotdot() {
        assert_eq!(normalize_lexically("docs/../regular.md"), "regular.md");
        assert_eq!(normalize_lexically("./docs/./link.md"), "docs/link.md");
        assert_eq!(normalize_lexically("a/b/../../c"), "c");
    }
}
