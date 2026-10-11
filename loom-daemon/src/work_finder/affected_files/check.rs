//! Curator's Affected-Files preflight (issue #9400): classify every path an
//! issue body lists under `## Affected Files` against the build base.
//!
//! The Builder works in a fresh worktree off the base ref, so a listed file
//! that exists only in the Curator's working tree would strand it. The old
//! check was a bash loop in the Curator prompt that treated *every* path
//! absent from the base as that case and parked the issue `loom:blocked` —
//! including files the issue itself creates. This module separates the cases:
//!
//! | Class | Condition | Verdict |
//! |---|---|---|
//! | `present` | on the base tree (also when reached through a symlink, or a bare file name matching a base file) | clear |
//! | `new` | absent from the base, marked new | clear |
//! | `unknown` | absent from the base and the working tree, unmarked | ask, do not park |
//! | `uncommitted` | absent from the base, present in the working tree, unmarked | park |
//!
//! # Marker grammar (defined here and nowhere else)
//!
//! A path is marked new when its line carries `(new)`, `(new file)`,
//! `(create)` or `(created)` outside backticks (case-insensitive), or when it
//! sits under a sub-heading of the Affected Files section whose title starts
//! with `New` (`### New files`).
//!
//! # Section walk
//!
//! [`super::affected_surface`] ends the section at the next heading of any
//! level, which is right for the overlap gate but would drop everything under
//! `### New files`. The walk here ends the section only at a heading of the
//! same or a shallower level; a deeper heading is a sub-section. The
//! path-shape rules ([`super::as_path`]) are shared, so both agree on what a
//! path is.
//!
//! No forge call is made: one `git ls-tree` of the base plus filesystem stats.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use super::{as_path, backticked};

/// Every listed path is `present` or `new`, or nothing is listed.
pub const EXIT_CLEAR: i32 = 0;
/// The check could not run (workspace is not a git repo, or the base ref does
/// not resolve). Never an all-clear, and never a reason to park.
pub const EXIT_COULD_NOT_RUN: i32 = 3;
/// Worst class is `unknown`: ask for clarification, do not park.
pub const EXIT_UNKNOWN: i32 = 10;
/// At least one path is `uncommitted`: the one parking verdict.
pub const EXIT_UNCOMMITTED: i32 = 11;

/// How a listed path relates to the build base. Ordered by severity, so the
/// maximum over a report is its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathClass {
    /// On the base tree.
    Present,
    /// Absent from the base and marked as created by the issue.
    New,
    /// Absent from the base and the working tree, unmarked: a file the issue
    /// creates without saying so, or a typo.
    Unknown,
    /// Absent from the base, present in the working tree, unmarked.
    Uncommitted,
}

impl PathClass {
    /// The word printed on stdout for this class.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::New => "new",
            Self::Unknown => "unknown",
            Self::Uncommitted => "uncommitted",
        }
    }
}

/// The classification of every listed path, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub entries: Vec<(String, PathClass)>,
}

impl Report {
    /// The exit code for the worst class in the report.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self.entries.iter().map(|(_, c)| *c).max() {
            Some(PathClass::Uncommitted) => EXIT_UNCOMMITTED,
            Some(PathClass::Unknown) => EXIT_UNKNOWN,
            Some(PathClass::Present | PathClass::New) | None => EXIT_CLEAR,
        }
    }

    /// One `<class>\t<path>` line per listed path.
    #[must_use]
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(path, class)| format!("{}\t{path}\n", class.as_str()))
            .collect()
    }
}

/// The level and lower-cased title of a markdown ATX heading line.
fn heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    let level = t.bytes().take_while(|b| *b == b'#').count();
    let rest = &t[level..];
    ((1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(char::is_whitespace)))
        .then(|| (level, rest.trim().to_ascii_lowercase()))
}

/// Whether `line` carries a new-file marker outside its backtick spans.
fn has_new_marker(line: &str) -> bool {
    let prose: String = line
        .split('`')
        .step_by(2)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    ["(new)", "(new file)", "(create)", "(created)"]
        .iter()
        .any(|m| prose.contains(m))
}

/// `path` normalised for lookup, or `None` when it cannot name one concrete
/// repo-relative file: a glob or placeholder, a URL, an `owner/repo#N`
/// reference, a command-line flag, or a path outside the repo.
fn concrete(path: &str) -> Option<String> {
    // `as_path` drops a `:42` suffix; a `:40-60` line range is cited as often.
    let p = match path.rsplit_once(':') {
        Some((head, tail))
            if tail.starts_with(|c: char| c.is_ascii_digit())
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-' || b == b',') =>
        {
            head
        }
        _ => path,
    };
    let p = p.trim_end_matches('/');
    let skip = p.is_empty()
        || p.starts_with(['/', '~', '-'])
        || p.contains("://")
        || p.contains(['*', '?', '<', '>', '{', '}', '[', ']', '$', '#', '…'])
        || p.split('/').any(|seg| seg == ".." || seg == "...");
    (!skip).then(|| p.to_string())
}

/// Every concrete path listed under an `Affected Files` heading, mapped to
/// whether it is marked new. Empty when the section is absent or names no
/// path ("To be determined").
#[must_use]
pub fn listed_paths(body: &str) -> BTreeMap<String, bool> {
    let mut section: Option<usize> = None;
    let mut new_sub = false;
    let mut listed = BTreeMap::new();
    for line in body.lines() {
        if let Some((level, title)) = heading(line) {
            match section {
                Some(top) if level > top => new_sub = title.starts_with("new"),
                _ => {
                    section = (level >= 2 && title.starts_with("affected files")).then_some(level);
                    new_sub = false;
                }
            }
            continue;
        }
        if section.is_none() {
            continue;
        }
        let marked = new_sub || has_new_marker(line);
        for path in backticked(line).filter_map(|s| as_path(&s).and_then(|p| concrete(&p))) {
            *listed.entry(path).or_insert(false) |= marked;
        }
    }
    listed
}

/// Classify `listed` given the two facts the check needs about a path.
pub fn classify(
    listed: &BTreeMap<String, bool>,
    on_base: impl Fn(&str) -> bool,
    in_tree: impl Fn(&str) -> bool,
) -> Report {
    let entries = listed
        .iter()
        .map(|(path, marked_new)| {
            let class = if on_base(path) {
                PathClass::Present
            } else if *marked_new {
                PathClass::New
            } else if in_tree(path) {
                PathClass::Uncommitted
            } else {
                PathClass::Unknown
            };
            (path.clone(), class)
        })
        .collect();
    Report { entries }
}

/// Run `git` in `dir`, returning stdout on success and a reason on failure.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Classify the Affected Files of `body` against `base` in the git checkout
/// containing `workspace`.
///
/// # Errors
///
/// A reason string when the check could not run: `workspace` is not inside a
/// git checkout, or `base` does not resolve to a tree. The caller must report
/// that as [`EXIT_COULD_NOT_RUN`], never as a verdict.
pub fn check(body: &str, workspace: &Path, base: &str) -> Result<Report, String> {
    if base.is_empty() || base.starts_with('-') {
        return Err(format!("invalid base ref {base:?}"));
    }
    let root = git(workspace, &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(root.trim_end_matches(['\n', '\r'])).to_path_buf();
    let listing = git(&root, &["ls-tree", "-r", "-z", "--name-only", base])
        .map_err(|e| format!("base ref {base:?} is unreachable: {e}"))?;

    let mut tree: BTreeSet<&str> = BTreeSet::new();
    let mut basenames: BTreeSet<&str> = BTreeSet::new();
    for file in listing.split('\0').filter(|f| !f.is_empty()) {
        tree.insert(file);
        basenames.insert(file.rsplit('/').next().unwrap_or(file));
        let mut dir = file;
        while let Some((parent, _)) = dir.rsplit_once('/') {
            if !tree.insert(parent) {
                break;
            }
            dir = parent;
        }
    }

    // A listed path may reach a base file through a committed symlink
    // (`.loom/roles/x.md` where `.loom/roles` links to `defaults/roles`):
    // `ls-tree` names only the link, so resolve the path on disk and look the
    // real location up instead of calling it uncommitted.
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let resolved_on_base = |path: &str| {
        root.join(path)
            .canonicalize()
            .ok()
            .and_then(|real| real.strip_prefix(&canon_root).ok().map(Path::to_path_buf))
            .and_then(|rel| rel.to_str().map(|r| tree.contains(r)))
            .unwrap_or(false)
    };

    // A slash-less span is as often shorthand for a file named in full
    // elsewhere ("lower the `curator.md` entry") as a root-level file, so one
    // that matches the basename of a base file is that file, not a miss.
    let mentions_base_file = |path: &str| !path.contains('/') && basenames.contains(path);

    Ok(classify(
        &listed_paths(body),
        |path| tree.contains(path) || mentions_base_file(path) || resolved_on_base(path),
        |path| root.join(path).symlink_metadata().is_ok(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "check_tests.rs"]
mod tests;
