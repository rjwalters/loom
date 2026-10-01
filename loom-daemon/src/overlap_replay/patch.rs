//! Git-derived PR outcomes (#9785 step 3): parse a unified diff into
//! changed-file records with line intervals, and derive a PR's *own* patch
//! without absorbing unrelated changes inherited through a base update.
//!
//! Two derivation modes, both explicit in the outcome record:
//!
//! * **range mode** — `diff merge-base(base, head) → head`. Exact when the
//!   branch still forks from `base` (the merge-base *is* the declared
//!   common source, so both pair sides map to one coordinate basis).
//!   Contaminated when the branch contains upstream commits (rebase onto an
//!   advanced base): the contaminated flag is set from commit ancestry, and
//!   when `own_commits` exist the derivation falls back to commit mode.
//! * **commit mode** — per `own_commits` entry, `git show` each commit
//!   against its first parent and union the intervals per file. Coordinates
//!   are per-commit-parent, so line-level cross-commit measurements are
//!   approximate; the record says so via `coordinate_basis`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// One changed file with its changed line intervals in *new* coordinates
/// (the head side), old coordinates kept for rename/deletion mapping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilePatch {
    /// Path on the head side (rename target when renamed).
    pub path: String,
    /// Prior path when the file was renamed or deleted, else equals `path`.
    pub path_before: String,
    pub status: PatchStatus,
    /// Changed intervals in new (head) coordinates. A pure deletion yields a
    /// zero-width anchor at the deletion point.
    pub intervals_new: Vec<Span>,
    /// Changed intervals in old (base) coordinates.
    pub intervals_old: Vec<Span>,
    pub added_lines: u32,
    pub deleted_lines: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PatchStatus {
    Added,
    Deleted,
    Renamed,
    Modified,
    Binary,
}

/// A half-open-at-the-end line span with zero-width anchor support:
/// `start == end + 1`-style anchors are modeled as `len == 0` at
/// [`Span::anchor`]. Inclusive on both ends for `len > 0`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    /// Last covered line (>= start). For a zero-width anchor, `end == start - 1`
    /// is normalized to `end == start` with `anchor = true` instead.
    pub end: u32,
    pub anchor: bool,
}

impl Span {
    /// Zero-width insertion/deletion point at `line`.
    pub fn anchor(line: u32) -> Self {
        Self {
            start: line,
            end: line,
            anchor: true,
        }
    }
    /// Inclusive interval `[start, end]` (end >= start).
    pub fn interval(start: u32, end: u32) -> Self {
        Self {
            start,
            end,
            anchor: false,
        }
    }
    pub fn len(&self) -> u32 {
        if self.anchor {
            0
        } else {
            self.end - self.start + 1
        }
    }
    /// A zero-width anchor is the "empty" span.
    pub fn is_empty(&self) -> bool {
        self.anchor
    }
    pub fn contains(&self, line: u32) -> bool {
        !self.anchor && line >= self.start && line <= self.end
    }
}

/// A PR's own changes, with the derivation provenance the score stage needs
/// to decide whether line coordinates are comparable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnPatch {
    pub pr: u32,
    pub base_sha: String,
    pub head_sha: String,
    /// The merge-base actually used in range mode (equals `base_sha` when the
    /// branch still forks from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_base: Option<String>,
    /// How the patch was derived.
    pub coordinate_basis: CoordinateBasis,
    /// Range mode found upstream commits inside `base..head` (the PR was
    /// rebased onto an advanced base and inherited their changes). With
    /// `own_commits` present the patch falls back to commit mode, so this
    /// flag then documents what *was* detected rather than what was counted.
    pub base_update_contamination: bool,
    pub files: Vec<FilePatch>,
    /// The raw unified diff the files were parsed from (identifier
    /// extraction evidence). Not serialized — recomputable from the pins.
    #[serde(skip)]
    pub raw_diff: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateBasis {
    /// All intervals share one coordinate basis: merge-base(base, head) —
    /// exact cross-PR line comparison.
    CommonSource,
    /// Intervals are per-commit-parent unions — file-level exact, line-level
    /// approximate.
    PerCommitParent,
}

/// Run `git` in `repo`, capturing stdout; `Err` on non-zero exit.
pub fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("running git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve a rev to a full commit SHA, failing loudly on unresolvable pins.
pub fn resolve_commit(repo: &Path, rev: &str) -> anyhow::Result<String> {
    let s = git(repo, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])?;
    let s = s.trim().to_string();
    if s.is_empty() {
        anyhow::bail!("cannot resolve commit {rev:?}");
    }
    Ok(s)
}

/// Parse `git diff` unified output into [`FilePatch`]s.
///
/// Expects `-U0`-style tight hunks but tolerates context (context lines only
/// advance counters). Handles `diff --git` headers, `rename from/to`,
/// `new file mode`, `deleted file mode`, `Binary files … differ`, and
/// `\ No newline at end of file`.
// The counters reset at every hunk close, including the final one where the
// reset is dead by construction — that trailing reset keeps the macro total.
#[allow(unused_assignments)]
pub fn parse_unified_diff(diff: &str) -> Vec<FilePatch> {
    let mut patches: Vec<FilePatch> = Vec::new();
    // Current file being parsed, and the open +/- runs inside the current
    // hunk (None = not inside a run; Some(start) = run began at that line).
    // `in_hunk` gates content handling: `---`/`+++`/`index` header lines and
    // everything between the `diff --git` header and the first `@@` are file
    // metadata, never diff content.
    let mut cur: Option<FilePatch> = None;
    let mut in_hunk = false;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut old_run: Option<u32> = None;
    let mut new_run: Option<u32> = None;
    let mut added_in_hunk = 0u32;
    let mut deleted_in_hunk = 0u32;

    // Close the current hunk: a deletion-only hunk registers a zero-width
    // anchor at the deletion point in new coordinates (the "before line N"
    // position git reports as `+N,0`), then any open runs close.
    macro_rules! close_hunk {
        () => {
            in_hunk = false;
            if let Some(p) = cur.as_mut() {
                if deleted_in_hunk > 0 && added_in_hunk == 0 {
                    p.intervals_new.push(Span::anchor(new_line));
                }
                if let Some(s) = old_run.take() {
                    p.intervals_old
                        .push(Span::interval(s, old_line.saturating_sub(1).max(s)));
                }
                if let Some(s) = new_run.take() {
                    p.intervals_new
                        .push(Span::interval(s, new_line.saturating_sub(1).max(s)));
                }
            }
            added_in_hunk = 0;
            deleted_in_hunk = 0;
        };
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            close_hunk!();
            if let Some(done) = cur.take() {
                patches.push(done);
            }
            let (b, a) = split_ab_paths(rest);
            cur = Some(FilePatch {
                path: a,
                path_before: b,
                status: PatchStatus::Modified,
                intervals_new: Vec::new(),
                intervals_old: Vec::new(),
                added_lines: 0,
                deleted_lines: 0,
            });
        } else if line.starts_with("@@ ") && cur.is_some() {
            close_hunk!();
            in_hunk = true;
            if let Some(hdr) = line.strip_prefix("@@ ") {
                if let Some((o, n)) = parse_hunk_header(hdr) {
                    old_line = o;
                    new_line = n;
                }
            }
        } else if let Some(p) = cur.as_mut() {
            if !in_hunk {
                if let Some(pp) = line.strip_prefix("rename from ") {
                    p.path_before = pp.to_string();
                    p.status = PatchStatus::Renamed;
                } else if let Some(pp) = line.strip_prefix("rename to ") {
                    p.path = pp.to_string();
                    p.status = PatchStatus::Renamed;
                } else if line.starts_with("new file mode") {
                    p.status = PatchStatus::Added;
                } else if line.starts_with("deleted file mode") {
                    p.status = PatchStatus::Deleted;
                } else if line.starts_with("Binary files ") {
                    p.status = PatchStatus::Binary;
                }
            } else if line.starts_with('+') {
                if new_run.is_none() {
                    new_run = Some(new_line);
                }
                new_line += 1;
                p.added_lines += 1;
                added_in_hunk += 1;
            } else if line.starts_with('-') {
                if old_run.is_none() {
                    old_run = Some(old_line);
                }
                old_line += 1;
                p.deleted_lines += 1;
                deleted_in_hunk += 1;
            } else if line.starts_with(' ') || line.is_empty() {
                close_hunk!();
            }
            // A "\ No newline at end of file" marker inside a hunk is a
            // marker, not a line — ignored.
        }
    }
    close_hunk!();
    if let Some(done) = cur.take() {
        patches.push(done);
    }
    normalize(patches)
}

/// Post-pass: sort intervals, drop pseudo-patches from stray headers.
fn normalize(mut patches: Vec<FilePatch>) -> Vec<FilePatch> {
    patches.retain(|p| !p.path.is_empty() && !p.path_before.is_empty());
    for p in &mut patches {
        p.intervals_new.sort_by_key(|s| (s.start, s.end));
        p.intervals_old.sort_by_key(|s| (s.start, s.end));
    }
    patches
}

/// `a/file b/file` → (before, after); strips `a/` and `b/`, handles quotes.
fn split_ab_paths(rest: &str) -> (String, String) {
    let parts = shellish_split(rest);
    let before = parts
        .first()
        .map(|s| unquote_path(s.strip_prefix("a/").unwrap_or(s)))
        .unwrap_or_default();
    let after = parts
        .get(1)
        .map(|s| unquote_path(s.strip_prefix("b/").unwrap_or(s)))
        .unwrap_or_default();
    (before, after)
}

fn unquote_path(p: &str) -> String {
    let p = p.trim();
    if p.len() >= 2 && p.starts_with('"') && p.ends_with('"') {
        let inner = &p[1..p.len() - 1];
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        p.to_string()
    }
}

fn shellish_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => in_q = !in_q,
            ' ' if !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `"-old_start[,old_count] +new_start[,new_count] …"` → (old_start,
/// new_start). Count 0 or missing does not shift the *start* interpretation:
/// git reports the line *before which* an empty side sits, which for our
/// interval purposes equals the reported start.
fn parse_hunk_header(hdr: &str) -> Option<(u32, u32)> {
    let mut parts = hdr.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let old_start: u32 = old.split(',').next()?.trim().parse().ok()?;
    let new_start: u32 = new.split(',').next()?.trim().parse().ok()?;
    Some((old_start, new_start))
}

/// Derive the PR's own patch from pinned SHAs in `repo`.
///
/// Mode selection: with `own_commits`, derive per-commit and union per file
/// (rebase-proof by construction). Without them, range mode off the
/// merge-base and flag contamination when `base..head` contains commits that
/// are ancestors of `origin/HEAD` (landed upstream — inherited, not the
/// PR's own work).
pub fn own_patch(
    repo: &Path,
    pr: u32,
    base_sha: &str,
    head_sha: &str,
    own_commits: Option<&[String]>,
) -> anyhow::Result<OwnPatch> {
    resolve_commit(repo, base_sha)?;
    resolve_commit(repo, head_sha)?;
    if let Some(commits) = own_commits.filter(|c| !c.is_empty()) {
        let mut merged: BTreeMap<String, FilePatch> = BTreeMap::new();
        for c in commits {
            let diff = git(repo, &["show", "--format=", "--find-renames", "-U0", c])?;
            for fp in parse_unified_diff(&diff) {
                let entry = merged.entry(fp.path.clone()).or_insert_with(|| fp.clone());
                entry.intervals_new.extend(fp.intervals_new.iter().copied());
                entry.intervals_old.extend(fp.intervals_old.iter().copied());
                entry.added_lines += fp.added_lines;
                entry.deleted_lines += fp.deleted_lines;
                if entry.status != fp.status && entry.status == PatchStatus::Modified {
                    entry.status = fp.status;
                }
                entry.path_before = fp.path_before;
            }
        }
        let mut files: Vec<FilePatch> = merged.into_values().collect();
        for f in &mut files {
            f.intervals_new.sort_by_key(|s| (s.start, s.end));
            f.intervals_old.sort_by_key(|s| (s.start, s.end));
        }
        return Ok(OwnPatch {
            pr,
            base_sha: base_sha.into(),
            head_sha: head_sha.into(),
            merge_base: None,
            coordinate_basis: CoordinateBasis::PerCommitParent,
            base_update_contamination: false,
            files,
            raw_diff: commits
                .iter()
                .map(|c| {
                    git(repo, &["show", "--format=", "--find-renames", "-U0", c])
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }

    let mb = git(repo, &["merge-base", base_sha, head_sha])?
        .trim()
        .to_string();
    // Contamination: a commit in base..head that is also an ancestor of the
    // repo's default branch head landed upstream — inherited, not ours.
    let upstream = git(repo, &["rev-parse", "--verify", "-q", "origin/HEAD^{commit}"])
        .or_else(|_| git(repo, &["rev-parse", "--verify", "-q", "main^{commit}"]))
        .unwrap_or_default();
    let mut contamination = false;
    if !upstream.trim().is_empty() {
        let ours = git(repo, &["log", "--format=%H", &format!("{base_sha}..{head_sha}")])?;
        for c in ours.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if git(repo, &["merge-base", "--is-ancestor", c, upstream.trim()]).is_ok() {
                contamination = true;
                break;
            }
        }
    }
    let diff = git(repo, &["diff", "--find-renames", "-U0", &mb, head_sha])?;
    let files = parse_unified_diff(&diff);
    Ok(OwnPatch {
        pr,
        base_sha: base_sha.into(),
        head_sha: head_sha.into(),
        merge_base: Some(mb),
        coordinate_basis: CoordinateBasis::CommonSource,
        base_update_contamination: contamination,
        files,
        raw_diff: diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = r#"diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,0 +11,2 @@ fn f() {
+    let x = 1;
+    let y = 2;
@@ -30,1 +32,1 @@ fn g() {
-    old();
+    new();
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
@@ -5,1 +5,2 @@ fn h() {
+    kept();
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
--- a/src/gone.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-a
-b
-c
diff --git a/src/img.bin b/src/img.bin
index 111..222 100644
Binary files a/src/img.bin and b/src/img.bin differ
"#;

    #[test]
    fn parses_hunks_renames_deletes_binary() {
        let ps = parse_unified_diff(DIFF);
        assert_eq!(ps.len(), 4);
        let a = &ps[0];
        assert_eq!(a.path, "src/a.rs");
        assert_eq!(a.added_lines, 3); // 2 in the first hunk + 1 in the second
        assert_eq!(a.deleted_lines, 1);
        assert_eq!(a.intervals_new[0], Span::interval(11, 12));
        assert_eq!(a.intervals_new[1], Span::interval(32, 32));
        assert_eq!(a.intervals_old[0], Span::interval(30, 30));
        let r = &ps[1];
        assert_eq!(r.status, PatchStatus::Renamed);
        assert_eq!(r.path, "src/new.rs");
        assert_eq!(r.path_before, "src/old.rs");
        let d = &ps[2];
        assert_eq!(d.status, PatchStatus::Deleted);
        assert_eq!(d.intervals_old.len(), 1);
        assert_eq!(d.intervals_old[0].len(), 3);
        assert_eq!(ps[3].status, PatchStatus::Binary);
    }

    #[test]
    fn hunk_header_edge_cases() {
        assert_eq!(parse_hunk_header("-10,0 +11,2 @@ fn"), Some((10, 11)));
        assert_eq!(parse_hunk_header("-1 +1,3 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("garbage"), None);
    }

    #[test]
    fn quoted_paths_unescape() {
        assert_eq!(unquote_path("\"a/b\\\"q.rs\""), "a/b\"q.rs");
        assert_eq!(unquote_path("\"a\\\\b.rs\""), "a\\b.rs");
        assert_eq!(unquote_path("a/b.rs"), "a/b.rs");
    }

    #[test]
    fn span_anchor_semantics() {
        let a = Span::anchor(7);
        assert_eq!(a.len(), 0);
        assert!(!a.contains(7));
        let i = Span::interval(5, 9);
        assert_eq!(i.len(), 5);
        assert!(i.contains(5) && i.contains(9) && !i.contains(10));
    }

    #[test]
    fn pure_deletion_gets_new_coord_anchor() {
        let diff = "diff --git a/src/x.rs b/src/x.rs\n\
                    index 111..222 100644\n\
                    --- a/src/x.rs\n\
                    +++ b/src/x.rs\n\
                    @@ -30,1 +32,0 @@ fn g() {\n\
                    -    old();\n";
        let ps = parse_unified_diff(diff);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].intervals_new, vec![Span::anchor(32)]);
        assert_eq!(ps[0].intervals_old, vec![Span::interval(30, 30)]);
    }

    #[test]
    fn whole_file_deletion_anchors_at_zero() {
        let diff = "diff --git a/src/g.rs b/src/g.rs\n\
                    deleted file mode 100644\n\
                    --- a/src/g.rs\n\
                    +++ /dev/null\n\
                    @@ -1,2 +0,0 @@\n\
                    -a\n\
                    -b\n";
        let ps = parse_unified_diff(diff);
        assert_eq!(ps[0].status, PatchStatus::Deleted);
        assert_eq!(ps[0].intervals_old[0].len(), 2);
        assert_eq!(ps[0].intervals_new[0], Span::anchor(0));
    }
}
