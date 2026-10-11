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
//! | `present` | on the base tree (also when reached through a symlink committed on the base), or a bare file name matching a base file that is not in the working tree | clear |
//! | `uncommitted` | absent from the base, present in the working tree (a new marker does not matter) | park |
//! | `new` | absent from the base and the working tree, marked new | clear |
//! | `unknown` | absent from the base and the working tree, unmarked | ask, do not park |
//!
//! A path in the working tree but not on the base is `uncommitted` whatever
//! its line says: a Builder that creates it in a fresh worktree would collide
//! with the Curator's untracked copy on `git pull`. Three rules keep that
//! true:
//!
//! * **Only symlinks committed on the base are followed**, with the target
//!   read from the base blob, and never out of the checkout. An untracked
//!   symlink is looked up under its own name, so it cannot borrow the status
//!   of the tracked file it points at.
//! * **The working-tree test matches the spelling exactly.** `readme.md` for
//!   a tracked `README.md` is `unknown` on every platform; a plain stat on a
//!   case-insensitive filesystem would find the tracked file and park. A path
//!   reached through a directory link that leaves the checkout is not in the
//!   working tree either.
//! * **A `#fragment` is not part of the file name.** `docs/a.md#usage` names
//!   `docs/a.md`, unless a file with that literal name exists. The file is
//!   reported only when it is on the base or in the working tree, so
//!   `owner/repo#12` stays unreported.
//!
//! # Marker grammar (defined here and nowhere else)
//!
//! A path is marked new when its line carries `(new)`, `(new file)`,
//! `(create)` or `(created)` outside backticks (case-insensitive), or when it
//! sits under a sub-heading of the Affected Files section whose title starts
//! with the whole word `New` (`### New files`, `### NEW: tests`; not
//! `### Newsletter` or `### Newer modules`).
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
//! A heading line inside a fenced code block (a `# comment` in a shell
//! snippet) is not a heading: it neither ends the section nor opens a
//! sub-section. Fences pair as in CommonMark: a run of three or more backticks
//! or tildes opens one, and only a run of the same character, at least as
//! long and with nothing after it, closes it. **A fence that is never closed
//! is not a fence**: its opening line is read as plain text, so a stray
//! opener above `## Affected Files` cannot hide the section. (CommonMark would
//! run it to the end of the document; hiding every listed path is the worse
//! mistake here.) Backtick-quoted paths on the lines inside a fence are still
//! listed.
//!
//! No forge call is made: one `git ls-tree` of the base, one `git cat-file`
//! per committed symlink a listed path crosses, and directory listings inside
//! the checkout. No listed file is read.

use std::cell::RefCell;
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
    /// On the base tree, directly or through a symlink committed there.
    Present,
    /// Absent from the base and the working tree, marked as created by the
    /// issue.
    New,
    /// Absent from the base and the working tree, unmarked: a file the issue
    /// creates without saying so, or a typo.
    Unknown,
    /// Absent from the base, present in the working tree.
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

/// The fence character, run length and remainder of a line that starts with
/// three or more backticks or tildes.
fn fence_run(line: &str) -> Option<(u8, usize, &str)> {
    let t = line.trim_start();
    let ch = t.bytes().next().filter(|b| matches!(b, b'`' | b'~'))?;
    let run = t.bytes().take_while(|b| *b == ch).count();
    (run >= 3).then(|| (ch, run, &t[run..]))
}

/// For each line, whether it opens, closes or sits inside a fenced code
/// block. An opener with no closing fence below it is not a fence (see the
/// module docs), so its line and the ones after it stay `false`.
fn fenced_lines(lines: &[&str]) -> Vec<bool> {
    let mut fenced = vec![false; lines.len()];
    // Per fence character, the shortest opener known to have no closer below
    // it. A later opener at least as long has none either, so it is not
    // searched for again and the walk stays linear in practice.
    let mut unclosed = [usize::MAX; 2];
    let mut i = 0;
    while i < lines.len() {
        // A backtick fence's info string cannot hold a backtick: that line
        // is an inline code span.
        let opener = fence_run(lines[i]).filter(|(ch, run, info)| {
            (*ch != b'`' || !info.contains('`')) && *run < unclosed[usize::from(*ch == b'~')]
        });
        let Some((ch, run, _)) = opener else {
            i += 1;
            continue;
        };
        let closer = lines[i + 1..].iter().position(|line| {
            fence_run(line)
                .is_some_and(|(c, r, rest)| c == ch && r >= run && rest.trim().is_empty())
        });
        if let Some(offset) = closer {
            let end = i + 1 + offset;
            fenced[i..=end].fill(true);
            i = end + 1;
        } else {
            unclosed[usize::from(ch == b'~')] = run;
            i += 1;
        }
    }
    fenced
}

/// Whether a sub-heading title (lower-cased) starts with the word `new`.
fn is_new_title(title: &str) -> bool {
    title
        .strip_prefix("new")
        .is_some_and(|rest| !rest.starts_with(char::is_alphanumeric))
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

/// Whether `path` carries glob or placeholder characters. Such a span is
/// usually a pattern, but `app/[id]/page.tsx` and `routes/$id.tsx` are real
/// file names, so [`check`] keeps it only when it exists literally. A `#` is
/// here for `owner/repo#12`; [`check`] also looks up the file a `#fragment`
/// is attached to.
fn glob_shaped(path: &str) -> bool {
    path.contains(['*', '?', '<', '>', '{', '}', '[', ']', '$', '#', '…'])
}

/// [`as_path`] for a span that may carry a `#fragment`. `as_path` reads the
/// extension of `notes.md#usage` as `md#usage` and drops the span, so the part
/// before the first `#` decides whether the span is path-shaped. The fragment
/// is put back: [`check`] decides whether the span or the file before the
/// fragment is the one that exists.
fn path_shaped(span: &str) -> Option<String> {
    span.split_once('#')
        .filter(|(_, fragment)| !fragment.contains(char::is_whitespace))
        .and_then(|(file, fragment)| Some(format!("{}#{fragment}", as_path(file)?)))
        .or_else(|| as_path(span))
}

/// `path` normalised for lookup, or `None` when it cannot name a repo-relative
/// file: a URL, a command-line flag, or a path outside the repo. Glob-shaped
/// spans are kept here; see [`glob_shaped`].
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
        || p.split('/').any(|seg| seg == ".." || seg == "...");
    (!skip).then(|| p.to_string())
}

/// Every path listed under an `Affected Files` heading, mapped to
/// whether it is marked new. Empty when the section is absent or names no
/// path ("To be determined").
#[must_use]
pub fn listed_paths(body: &str) -> BTreeMap<String, bool> {
    let mut section: Option<usize> = None;
    let mut new_sub = false;
    let mut listed = BTreeMap::new();
    let lines: Vec<&str> = body.lines().collect();
    for (line, fenced) in lines.iter().zip(fenced_lines(&lines)) {
        if fenced {
            // The fence line itself names no path (` ```src/x.rs ` is an info
            // string); a heading-shaped line inside the block is a comment.
            if fence_run(line).is_some() {
                continue;
            }
        } else if let Some((level, title)) = heading(line) {
            match section {
                Some(top) if level > top => new_sub = is_new_title(&title),
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
        for path in backticked(line).filter_map(|s| path_shaped(&s).and_then(|p| concrete(&p))) {
            *listed.entry(path).or_insert(false) |= marked;
        }
    }
    listed
}

/// Classify `listed` given the facts the check needs about a path: whether it
/// is on the base, whether it is in the working tree, and whether it is a bare
/// name matching a base file (`by_name`, only ever `unknown` to `present`).
pub fn classify(
    listed: &BTreeMap<String, bool>,
    on_base: impl Fn(&str) -> bool,
    in_tree: impl Fn(&str) -> bool,
    by_name: impl Fn(&str) -> bool,
) -> Report {
    let entries = listed
        .iter()
        .map(|(path, marked_new)| {
            let class = if on_base(path) {
                PathClass::Present
            } else if in_tree(path) {
                PathClass::Uncommitted
            } else if *marked_new {
                PathClass::New
            } else if by_name(path) {
                PathClass::Present
            } else {
                PathClass::Unknown
            };
            (path.clone(), class)
        })
        .collect();
    Report { entries }
}

/// Whether `path` exists under `root` (canonical) spelled exactly as written.
/// A plain stat would also accept `readme.md` for a tracked `README.md` on a
/// case-insensitive filesystem and call it uncommitted. A directory link that
/// leaves the checkout is not descended: what is behind it is not part of the
/// working tree.
fn exists_exactly(root: &Path, path: &str) -> bool {
    let mut dir = root.to_path_buf();
    path.split('/').all(|seg| {
        let found = dir.canonicalize().is_ok_and(|real| real.starts_with(root))
            && std::fs::read_dir(&dir)
                .is_ok_and(|mut entries| entries.any(|e| e.is_ok_and(|e| e.file_name() == seg)));
        dir.push(seg);
        found
    })
}

/// `path` with every symlink that is committed on the base followed, or
/// `None` when a link leaves the checkout, loops, or cannot be read.
///
/// `target_of` returns the base's target for a path that is a symlink on the
/// base and `None` for any other path. A symlink that exists only in the
/// working tree is therefore left alone: it is looked up under its own name
/// and cannot borrow the status of its target.
fn through_base_links(path: &str, target_of: impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut todo: Vec<String> = path.split('/').rev().map(str::to_string).collect();
    let mut done: Vec<String> = Vec::new();
    let mut hops = 0;
    while let Some(seg) = todo.pop() {
        match seg.as_str() {
            "" | "." => continue,
            ".." => {
                done.pop()?;
                continue;
            }
            _ => done.push(seg),
        }
        if let Some(target) = target_of(&done.join("/")) {
            hops += 1;
            if target.starts_with('/') || hops > 16 {
                return None;
            }
            done.pop();
            todo.extend(target.split('/').rev().map(str::to_string));
        }
    }
    Some(done.join("/"))
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
    let root = root.canonicalize().unwrap_or(root);
    let listing = git(&root, &["ls-tree", "-r", "-z", base])
        .map_err(|e| format!("base ref {base:?} is unreachable: {e}"))?;

    let mut tree: BTreeSet<&str> = BTreeSet::new();
    // The base's own symlinks, each mapped to the blob holding its target.
    let mut links: BTreeMap<&str, &str> = BTreeMap::new();
    let mut basenames: BTreeSet<&str> = BTreeSet::new();
    // Each entry is `<mode> <type> <object>\t<path>`; mode 120000 is a symlink.
    for (meta, file) in listing.split('\0').filter_map(|e| e.split_once('\t')) {
        tree.insert(file);
        if let Some(blob) = meta.strip_prefix("120000 blob ") {
            links.insert(file, blob);
        }
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
    // `ls-tree` names only the link, so follow the base's links and look the
    // real location up. The target comes from the base blob, not from the
    // working tree, where the link may have been edited or replaced.
    let targets: RefCell<BTreeMap<&str, Option<String>>> = RefCell::new(BTreeMap::new());
    let target_of = |path: &str| {
        let (&link, blob) = links.get_key_value(path)?;
        targets
            .borrow_mut()
            .entry(link)
            .or_insert_with(|| git(&root, &["cat-file", "blob", blob]).ok())
            .clone()
    };

    let on_base = |path: &str| {
        tree.contains(path)
            || through_base_links(path, target_of).is_some_and(|real| tree.contains(real.as_str()))
    };
    let in_tree = |path: &str| exists_exactly(&root, path);
    // A slash-less span is as often shorthand for a file named in full
    // elsewhere ("lower the `curator.md` entry") as a root-level file, so one
    // that matches the basename of a base file is that file, not a miss. It
    // is consulted only after the working tree, so it cannot hide a root file
    // that exists nowhere but there.
    let mentions_base_file = |path: &str| !path.contains('/') && basenames.contains(path);

    let exists = |path: &str| on_base(path) || in_tree(path);
    let mut listed = listed_paths(body);
    // `docs/a.md#usage` names `docs/a.md`, unless the whole span is a file.
    // The file is listed only when it exists, so `owner/repo#12` is not.
    let anchored: Vec<(String, bool)> = listed
        .iter()
        .filter(|(path, _)| !exists(path))
        .filter_map(|(path, marked)| Some((concrete(path.split_once('#')?.0)?, *marked)))
        .filter(|(file, _)| exists(file))
        .collect();
    for (file, marked) in anchored {
        *listed.entry(file).or_insert(false) |= marked;
    }
    // A glob-shaped span is a pattern unless it names a real file.
    listed.retain(|path, _| !glob_shaped(path) || exists(path));

    Ok(classify(&listed, on_base, in_tree, mentions_base_file))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "check_tests.rs"]
mod tests;
