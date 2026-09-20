//! Shared plumbing for `worktree.sh`'s WIP-shelving verbs — `snapshot`,
//! `stash-push` and `stash-pop` (#8195, epic #7810 slice 2).
//!
//! # Why these three, and why together
//!
//! They are one family: three verbs that take a worktree's uncommitted work
//! somewhere safe and (for two of them) put it back. They share a target
//! grammar (`<issue-number>` or the literal `main`), a path layout rooted at
//! [`crate::worktree_root::worktree_root`], the same "which untracked files
//! are Loom's own bookkeeping" filter, and the same stdout-purity split under
//! `--json`. Porting one without the others would have duplicated all four.
//!
//! They are also the part of `worktree.sh` whose *entire reason for existing*
//! is to avoid destroying somebody's work. `refs/stash` is repo-global across
//! every linked worktree, so two builders stashing at the same time in
//! different `issue-<N>` worktrees can pop each other's WIP (#4821); these
//! verbs exist because that happened for real. `stash-push` additionally runs
//! `git reset --hard HEAD` — an irreversible operation whose safety depends
//! entirely on the capture that precedes it having worked.
//!
//! # What shell made hard here, specifically
//!
//! Every path in this family is *data*: a worktree path, a repo root, an
//! untracked filename. In bash each of those has to survive word splitting at
//! every interpolation, and #7858 is what happens when one of them does not —
//! an unquoted path containing a space turned an orphan-guard cleanup into an
//! `rm -rf` on a live worktree. `std::process::Command::arg` does not word-split,
//! so the class is gone by construction rather than by review; `wip::tests`
//! pins that with a worktree root, a worktree and a file whose names all
//! contain spaces and shell metacharacters.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Which tree a verb operates on.
///
/// `main` is the primary clone (#6076): roles that legitimately run there
/// (Judge, Champion, Auditor, Guide, Hermit) had no sanctioned
/// clean-and-restore pair before it, so they reached for raw `git stash` and
/// hit an unanswerable guard ask on the pop half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Issue(u32),
    Main,
}

impl Target {
    /// Parse the target token exactly as the shell's
    /// `^[0-9]+$ || == "main"` test did.
    ///
    /// The `main` comparison is case-SENSITIVE on purpose and the retained
    /// suite asserts it: `MAIN` must be rejected. A case-folded match would
    /// make `Main`/`MAIN` silently operate on the primary clone, and the one
    /// tree where a mistaken `git reset --hard` is least recoverable is
    /// exactly that one.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if raw == "main" {
            return Some(Target::Main);
        }
        if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
            return raw.parse::<u32>().ok().map(Target::Issue);
        }
        None
    }

    /// Path/ref component: `issue-<N>` or `main`.
    #[must_use]
    pub fn slug(&self) -> String {
        match self {
            Target::Issue(n) => format!("issue-{n}"),
            Target::Main => "main".to_string(),
        }
    }

    /// The token as the caller typed it, for messages.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Target::Issue(n) => n.to_string(),
            Target::Main => "main".to_string(),
        }
    }

    /// The `issueNumber` JSON field. `main` reports `null` rather than the
    /// string `"main"` so an existing consumer cannot mis-parse it as a number
    /// (the retained suite asserts both halves).
    #[must_use]
    pub fn json_issue(&self) -> String {
        match self {
            Target::Issue(n) => n.to_string(),
            Target::Main => "null".to_string(),
        }
    }

    /// `refs/loom/stash-baseline/<slug>` — deliberately NOT `refs/stash`.
    #[must_use]
    pub fn baseline_ref(&self) -> String {
        format!("refs/loom/stash-baseline/{}", self.slug())
    }
}

/// Where a verb's output goes.
///
/// Under `--json` every human-readable line moves to stderr so stdout carries
/// exactly one JSON document — the retained suite asserts a single stdout line
/// — while errors go to stderr in both modes, as they always did.
pub struct Out {
    pub json: bool,
}

const RED: &str = "\x1b[0;31m";
const GREEN: &str = "\x1b[0;32m";
const BLUE: &str = "\x1b[0;34m";
const NC: &str = "\x1b[0m";

impl Out {
    #[must_use]
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    /// Always stderr, in both modes — matching `print_error`.
    pub fn error(msg: &str) {
        eprintln!("{RED}ERROR: {msg}{NC}");
    }

    pub fn info(&self, msg: &str) {
        if self.json {
            eprintln!("{BLUE}ℹ {msg}{NC}");
        } else {
            println!("{BLUE}ℹ {msg}{NC}");
        }
    }

    pub fn success(&self, msg: &str) {
        if self.json {
            eprintln!("{GREEN}✓ {msg}{NC}");
        } else {
            println!("{GREEN}✓ {msg}{NC}");
        }
    }

    /// Emit a JSON document, but only in `--json` mode. Mirrors the shell's
    /// `_snap_json` / `_sbp_json` / `_sbo_json` guards, which return early
    /// when `json` is false.
    pub fn json_line(&self, doc: &str) {
        if self.json {
            println!("{doc}");
        }
    }
}

/// Resolve the main workspace root the way every verb in this family does:
/// from the git COMMON dir, never from cwd.
///
/// `--git-common-dir`'s parent is the main workspace even when the process is
/// running inside a linked worktree, which is what makes `stash-push main`
/// work from a worktree cwd (the retained suite drives exactly that).
/// `--git-dir` would resolve per-worktree and silently target the wrong tree.
///
/// Returns `None` when not inside a repository at all.
#[must_use]
pub fn repo_root_from_cwd() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if common.is_empty() {
        return None;
    }
    // `dirname` semantics: a bare `.git` has parent `.`, i.e. cwd.
    let parent = Path::new(&common).parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Some(absolutize(parent))
}

/// Make a path absolute WITHOUT resolving symlinks.
///
/// Deliberately not `canonicalize`: the shell resolved this with
/// `cd "$(dirname …)" && pwd`, which is bash's *logical* pwd and keeps
/// symlinked prefixes intact (`/tmp` -> `/private/tmp` on macOS is the common
/// one). Canonicalizing would print a different `patchPath` than the shell did
/// for the same input, which is a stdout-contract change for no benefit.
fn absolutize(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| p.to_path_buf(), |cwd| cwd.join(p))
    };
    lexical_normalize(&joined)
}

/// Drop `.` components and fold `..` lexically, so `/repo/.git/..` renders as
/// `/repo` the way `cd … && pwd` would.
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// The three paths every verb needs, resolved once.
pub struct Layout {
    /// The main workspace (the git common dir's parent).
    pub repo_root: PathBuf,
    /// `loom_worktree_root(repo_root)` — honours `LOOM_WORKTREE_ROOT` and
    /// `.loom/config.json`'s `worktree.root`, so an overridden base redirects
    /// snapshots and baselines along with the worktrees themselves.
    pub worktree_root: PathBuf,
    /// The tree the verb actually operates on: an `issue-<N>` worktree, or the
    /// primary clone for `main`.
    pub worktree_path: PathBuf,
}

impl Layout {
    /// Resolve the layout for `target`, or `None` when not in a repository.
    #[must_use]
    pub fn resolve(target: &Target) -> Option<Self> {
        let repo_root = repo_root_from_cwd()?;
        let worktree_root = crate::worktree_root::worktree_root(&repo_root);
        let worktree_path = match target {
            Target::Main => repo_root.clone(),
            Target::Issue(n) => worktree_root.join(format!("issue-{n}")),
        };
        Some(Self {
            repo_root,
            worktree_root,
            worktree_path,
        })
    }

    /// `<worktree-root>/.stash-baseline/<slug>`: the holding directory for
    /// untracked files and the pending marker. Outside the worktree on
    /// purpose, so removing the worktree mid-capture strands nothing.
    #[must_use]
    pub fn holding_dir(&self, target: &Target) -> PathBuf {
        self.worktree_root
            .join(".stash-baseline")
            .join(target.slug())
    }
}

/// Run git in `dir` and hand back the completed output, or `None` if git could
/// not be executed at all.
pub fn git(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()
}

/// Whether git ran AND exited 0.
pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir, args).is_some_and(|o| o.status.success())
}

/// Trimmed stdout of a successful git invocation.
pub fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git(dir, args)?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Is `dir` a git working tree? Mirrors the shell's
/// `git -C "$p" rev-parse --git-dir` probe.
#[must_use]
pub fn is_git_worktree(dir: &Path) -> bool {
    git_ok(dir, &["rev-parse", "--git-dir"])
}

/// Does `ref_name` resolve in this tree?
#[must_use]
pub fn ref_exists(dir: &Path, ref_name: &str) -> bool {
    git_ok(dir, &["rev-parse", "--verify", "--quiet", ref_name])
}

/// Untracked, non-ignored files in `dir`, with Loom's own runtime markers
/// filtered out.
///
/// The filter is the point: `.loom-managed` is written into every managed
/// worktree by `worktree.sh` itself, and `.loom-in-use` / `.loom-checkpoint` /
/// `.no-changes-needed` are lifecycle signals other parts of Loom read off
/// disk. Folding them into a snapshot would capture noise; MOVING them out (as
/// `stash-push --include-untracked` would) actively breaks the worktree —
/// losing `.loom-managed` makes every cleanup path refuse the worktree
/// (#3548), and the retained suite asserts all four survive a push.
#[must_use]
pub fn untracked_files(dir: &Path) -> Vec<String> {
    let Some(out) = git(dir, &["ls-files", "--others", "--exclude-standard"]) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| !crate::worktree_ops::safety::is_loom_own_untracked_path(l))
        .map(std::string::ToString::to_string)
        .collect()
}

/// Move one file, falling back to copy-then-delete across filesystems.
///
/// `fs::rename` is `rename(2)`, which fails with `EXDEV` when source and
/// destination are on different filesystems — and they routinely are here,
/// because `LOOM_WORKTREE_ROOT` exists precisely to put the worktree base on
/// another volume (#3530). The shell used `mv`, which has always handled that.
/// Returns false on any failure, matching the shell's `mv … || continue`.
pub fn move_file(src: &Path, dest: &Path) -> bool {
    if std::fs::rename(src, dest).is_ok() {
        return true;
    }
    if std::fs::copy(src, dest).is_err() {
        return false;
    }
    if std::fs::remove_file(src).is_err() {
        // The copy landed but the original is still there. Leaving both would
        // mean `stash-push` reported a file as moved while it is still dirtying
        // the worktree, so undo the copy and report failure.
        let _ = std::fs::remove_file(dest);
        return false;
    }
    true
}

/// UTC timestamp in the snapshot filename's format.
#[must_use]
pub fn snapshot_stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// UTC timestamp in the pending marker's format.
#[must_use]
pub fn iso_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Render a path into a JSON string value.
///
/// A worktree path is attacker-adjacent data (it contains a repo name, and
/// under `LOOM_WORKTREE_ROOT` an operator-supplied prefix), and the shell
/// interpolated it into `printf '…"%s"…'` raw — a path containing `"` or a
/// backslash produced a document no consumer could parse. Everything else
/// about the document's shape is preserved byte-for-byte; only the escaping
/// is new.
#[must_use]
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Path as a lossy string, for messages and JSON.
#[must_use]
pub fn display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests;
