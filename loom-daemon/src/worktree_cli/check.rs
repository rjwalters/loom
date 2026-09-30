//! `worktree.sh`'s "am I inside a linked worktree?" predicate, and both of the
//! decisions it gates (#8195 slice 11, epic #7810).
//!
//! # The defect: the predicate was constant-true
//!
//! `check_if_in_worktree` asked git two questions and compared the answers as
//! strings:
//!
//! ```text
//! git_dir=$(git rev-parse --git-common-dir)
//! work_dir=$(git rev-parse --show-toplevel)
//! [[ "$git_dir" != "$work_dir/.git" ]]        # => "in a worktree"
//! ```
//!
//! `--show-toplevel` is always **absolute**; `--git-common-dir` is **relative
//! to the current directory** whenever it can be (`.git` at the repo root,
//! `../.git` one level down, `../../.git` two). So in the main checkout the
//! comparison is `.git` != `/repo/.git` — true — and the predicate answers *"in
//! a worktree"*. In a real linked worktree git returns the common dir as an
//! absolute path, which also differs from `<worktree>/.git`, so it answers
//! *"in a worktree"* there too. **The function has no false branch reachable
//! from anywhere a caller can stand**, verified against git 2.43 from the repo
//! root, a subdirectory, a linked worktree and a subdirectory of one.
//!
//! Two consumers, two consequences:
//!
//! 1. **`worktree.sh --check`** — a documented verb (`worktree-return.sh` tells
//!    operators to run it) — reported the primary clone as a worktree and
//!    exited 0. Its `else` arm, *"Not currently in a worktree (you're in the
//!    main working directory)"* and its exit 1, were dead code.
//! 2. **The create path's auto-navigation** printed four spurious lines on
//!    every `worktree.sh <N>` run from the primary clone — a yellow *"Currently
//!    in a worktree, auto-navigating to main workspace…"*, a `Current
//!    worktree:` block naming the primary clone, *"Found main workspace: ."*
//!    and *"Switched to main workspace"* — and then `cd`'d to
//!    `dirname ".git"` = `.`, which is a no-op *by luck*: git's relative answer
//!    happens to be the relative path to the repo root, so `dirname` of it is
//!    too.
//!
//! That luck is why this survived: under `--json` every one of those messages
//! is suppressed, so nothing a machine reads ever changed, and the human noise
//! looked like a feature. It is the same class as the defects slices 5, 8 and
//! 10 retired — **a path compared logically instead of physically** — and the
//! same shape of evidence gap: no retained suite asserts either consumer's
//! output, so nothing failed.
//!
//! # The predicate this uses instead
//!
//! git's own definition of a linked worktree: its `--git-dir` is a
//! *per-worktree* administrative directory (`<common>/worktrees/<name>`), while
//! the main checkout's `--git-dir` **is** the common dir. So
//!
//! ```text
//! canonicalize(--git-dir) != canonicalize(--git-common-dir)   <=> linked worktree
//! ```
//!
//! Both sides are resolved to absolute and canonicalized, so a repo reached
//! through a symlink (the case slice 10 found this same comparison failing on)
//! answers correctly, and a `--separate-git-dir` main checkout — where `.git`
//! is a *file*, so the tempting `[[ -f .git ]]` shortcut misfires — is
//! correctly *not* a worktree.
//!
//! # Arms
//!
//! [`report`] is the `--check` verb: the same two output shapes and the same
//! 0/1 exit codes, now with the second one reachable.
//!
//! [`porcelain`] is the create path's arm. It emits a `LEVEL<TAB>text` record
//! stream — the shape `merge-pr delete-branch` (#8973) and `merge-pr
//! dirty-guard` (#9149) established — which `worktree.sh` replays through its
//! own `print_warning` / `print_info` / `echo`, so every message and its order
//! stay owned here while the `cd` (which only the parent process can perform)
//! stays in the shell. `--quiet` is `--json` mode: only the two data records
//! are emitted, matching the shell's own blanket suppression.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How the shell wrapper is to print a replayed line.
///
/// The protocol names the **shell function**, not a colour, for the reason
/// [`crate::merge_pr::dirty_guard::Level`] gives: `worktree.sh` interleaves
/// these lines with dozens of its own and its suites read that formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Replay through `print_warning` (yellow, `⚠`).
    Warning,
    /// Replay through `print_info` (blue, `ℹ`).
    Info,
    /// Replay through a bare `echo` — `get_worktree_info`'s lines were
    /// uncoloured and their two-space indent is load-bearing.
    Plain,
    /// Replay as `echo ""`. A separate token rather than an empty [`Level::Plain`]
    /// so the record still carries a non-empty first field and cannot be
    /// mistaken for a blank line in the stream itself.
    Blank,
    /// Data, not a message: the caller is inside a linked worktree.
    InWorktree,
    /// Data, not a message: the absolute main-workspace directory to `cd` into.
    ///
    /// Absent — even when [`Level::InWorktree`] was emitted — when the common
    /// dir could not be resolved, which is the shell's *"Failed to find git
    /// common directory"* arm.
    MainWorkspace,
}

impl Level {
    /// The protocol token, exactly as the wrapper's `case` reads it.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Level::Warning => "WARNING",
            Level::Info => "INFO",
            Level::Plain => "PLAIN",
            Level::Blank => "BLANK",
            Level::InWorktree => "IN_WORKTREE",
            Level::MainWorkspace => "MAIN_WORKSPACE",
        }
    }
}

/// Where a process stands, physically, relative to the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// True iff the current directory is inside a **linked** worktree.
    pub linked_worktree: bool,
    /// `git rev-parse --show-toplevel`, verbatim — this is display text, and
    /// the retired `get_worktree_info` printed git's answer without resolving
    /// it. Canonicalizing it here would change the verb's output on a host
    /// whose repo path contains a symlink.
    pub worktree_path: Option<String>,
    /// `git rev-parse --abbrev-ref HEAD`, verbatim (`HEAD` when detached, as
    /// before).
    pub branch: Option<String>,
    /// The directory holding the common git dir — the main workspace, resolved
    /// absolutely. `None` when git could not answer at all, which is the
    /// shell's *"Failed to find git common directory"* arm.
    pub main_workspace: Option<PathBuf>,
}

/// Answer the predicate and collect everything either arm needs.
///
/// Every git call is `git -C <cwd>`, so the answer is about `cwd` and not about
/// this process's own working directory — which is what lets the tests drive
/// four positions (primary clone, a subdirectory of it, a linked worktree, a
/// subdirectory of one) without mutating process-global state.
#[must_use]
pub fn locate(cwd: &Path) -> Location {
    let git_dir = resolve_dir(cwd, "--git-dir");
    let common_dir = resolve_dir(cwd, "--git-common-dir");

    // The one comparison this module exists for. Both sides are canonicalized
    // absolute paths, so it is physical: `None` on either side means git could
    // not answer, which is never evidence OF a linked worktree.
    let linked_worktree = match (&git_dir, &common_dir) {
        (Some(gd), Some(cd)) => gd != cd,
        _ => false,
    };

    if !linked_worktree {
        return Location {
            linked_worktree: false,
            worktree_path: None,
            branch: None,
            main_workspace: None,
        };
    }

    Location {
        linked_worktree: true,
        worktree_path: git_line(cwd, &["rev-parse", "--show-toplevel"]),
        branch: git_line(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
        // The main workspace is the parent of `<repo>/.git`. The retired shell
        // took `dirname` of git's RAW answer, which for the (unreachable, but
        // now reachable) relative case would have produced a relative path;
        // the canonicalized parent is always absolute, which is what the
        // caller's `cd` wants and what the shell's own error text promises.
        main_workspace: common_dir.and_then(|d| d.parent().map(Path::to_path_buf)),
    }
}

/// The `--check` verb: `worktree.sh`'s `get_worktree_info`, byte for byte.
///
/// Exit 0 = inside a linked worktree, 1 = the main working directory. Both are
/// **answers**; there is no error path, which is why the stub reserves 2 for
/// "could not run at all".
pub fn report(cwd: &Path) -> i32 {
    let loc = locate(cwd);
    if !loc.linked_worktree {
        println!("Not currently in a worktree (you're in the main working directory)");
        return 1;
    }
    println!("Current worktree:");
    println!("  Path: {}", loc.worktree_path.unwrap_or_default());
    println!("  Branch: {}", loc.branch.unwrap_or_default());
    0
}

/// The create path's arm: the record stream described in the module docs.
///
/// Exit 0, always. "Not in a worktree" is the overwhelmingly common answer and
/// is not a failure, and the one genuine fault the shell reports — the common
/// dir being unresolvable — is signalled by the *absence* of a
/// [`Level::MainWorkspace`] record rather than by an exit code, because the
/// shell needs to print its own message and JSON document for it either way.
pub fn porcelain(cwd: &Path, quiet: bool) -> i32 {
    for (level, text) in records(&locate(cwd), quiet) {
        println!("{}\t{}", level.token(), text);
    }
    0
}

/// The records [`porcelain`] prints, as data — the seam the differential
/// harness and the unit tests compare on.
///
/// Order is the retired shell's message order exactly: the warning, a blank,
/// `get_worktree_info`'s three lines, a blank, then *"Found main workspace"*.
/// The two data records come last so a caller that reads the stream
/// line-by-line has already replayed every message before it acts.
#[must_use]
pub fn records(loc: &Location, quiet: bool) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    if !loc.linked_worktree {
        return out;
    }
    if !quiet {
        out.push((
            Level::Warning,
            "Currently in a worktree, auto-navigating to main workspace...".to_string(),
        ));
        out.push((Level::Blank, String::new()));
        out.push((Level::Plain, "Current worktree:".to_string()));
        out.push((
            Level::Plain,
            format!("  Path: {}", loc.worktree_path.clone().unwrap_or_default()),
        ));
        out.push((Level::Plain, format!("  Branch: {}", loc.branch.clone().unwrap_or_default())));
        out.push((Level::Blank, String::new()));
        if let Some(ws) = &loc.main_workspace {
            out.push((Level::Info, format!("Found main workspace: {}", ws.display())));
        }
    }
    out.push((Level::InWorktree, "true".to_string()));
    if let Some(ws) = &loc.main_workspace {
        out.push((Level::MainWorkspace, ws.display().to_string()));
    }
    out
}

/// `git -C <cwd> rev-parse <flag>`, resolved to a canonical absolute directory.
///
/// git answers these two flags relative to the current directory when it can,
/// so the result is joined onto `cwd` before canonicalizing — the exact step
/// the retired string comparison omitted. `None` when git cannot answer (not a
/// repository) or when the answer names a directory that does not exist.
fn resolve_dir(cwd: &Path, flag: &str) -> Option<PathBuf> {
    let raw = git_line(cwd, &["rev-parse", flag])?;
    let joined = cwd.join(raw);
    // `canonicalize` is what makes the comparison symlink-proof; falling back
    // to the un-resolved join keeps the predicate usable on a host where it
    // fails (a permission-denied component), where the two sides are still
    // compared consistently with each other.
    Some(std::fs::canonicalize(&joined).unwrap_or(joined))
}

/// One trimmed line of git stdout, or `None` when git failed or said nothing.
fn git_line(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests;
