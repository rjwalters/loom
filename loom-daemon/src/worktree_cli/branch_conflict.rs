//! `worktree.sh`'s "feature branch checked out in the main worktree" recovery
//! guard (#8195 slice 7, epic #7810) — `_handle_feature_branch_in_main_worktree`,
//! the arm `_try_worktree_add` falls into when `git worktree add` refuses with
//! git's own `fatal: '<branch>' is already used by worktree at '<path>'`.
//!
//! # What moved here
//!
//! This happens when a previous builder manually checked out `feature/issue-N`
//! in the main workspace and left it there. The recovery, in the shell's own
//! order (the order is observable — each rung returns before the next runs):
//!
//! 1. Is this even that error? A plain substring test on git's stderr.
//! 2. Extract the conflicting worktree's path from the quoted segment of that
//!    error text.
//! 3. Is the conflicting worktree the **main workspace** (not some other
//!    feature worktree)? Only the main workspace is safe to auto-switch.
//! 4. Does the main workspace have uncommitted changes? If so, refuse —
//!    auto-switching would discard them.
//! 5. Otherwise: `git checkout` the main workspace back to the default branch
//!    and tell the caller to retry.
//!
//! # Why this family
//!
//! It is the one arm of the create path that is **pure string parsing of an
//! arbitrary git error message** — a `grep -o '...' | sed 's/.../'` extraction
//! of a quoted path, then a raw string comparison against another path. That
//! is exactly #7858's class: a guard whose path handling silently does the
//! wrong thing (there, an unquoted `rm -rf` target; here, a path comparison a
//! trailing slash could quietly defeat). In Rust the extraction is a manual
//! byte scan with no shell word-splitting to get wrong, and the comparison is
//! [`std::path::PathBuf`] equality after both sides run through the same
//! **logical** (non-symlink-resolving) `cd … && pwd` the shell performed —
//! see [`resolve_dir_or_literal`] for why physically resolving here would be
//! a real, if subtle, behaviour change rather than a preservation.
//!
//! # Exit codes — the shell's three, unchanged
//!
//! | code | meaning |
//! |---|---|
//! | 0 | handled — a message was already printed (unless `--quiet`); do not retry |
//! | 1 | **not this error** — print nothing; the caller already has git's raw error text and reports it itself |
//! | 2 | auto-recovered — the main workspace was switched back to the default branch; retry `git worktree add` once |
//!
//! **A missing binary degrades to 1**, which the shell wrapper enforces itself
//! rather than leaving to a resolution failure: an unrecovered `git worktree
//! add` failure, reporting the raw git error, is exactly what happened before
//! this recovery existed. 0 would falsely claim a message was printed when
//! none was; 2 would falsely claim the main workspace was switched. Neither
//! side effect nor message ever happens without the binary, so 1 — "nothing
//! happened, here is the original error" — is the only truthful answer.
//!
//! # Why clap here, like `worktree-link` / `worktree-cleanup`
//!
//! Exactly one caller — a generated command line inside `worktree.sh` — and no
//! human types it, so clap's own usage error is the right answer for a
//! malformed invocation.
//!
//! # Behaviour deliberately preserved from the shell
//!
//! - **Every message is gated on `--quiet`, including the `print_error`
//!   calls.** This differs from [`super::wip::Out::error`], which several
//!   sibling modules route through unconditionally: the retired shell function
//!   wrapped its *entire* body — `print_error` included — in a single
//!   `if [[ "$JSON_OUTPUT" != "true" ]]`, so under `--json` this prints
//!   nothing at all, not even to stderr.
//! - The main workspace root is **passed in** (`--repo-root`), not re-derived
//!   via `git rev-parse --git-common-dir` the way the shell did. By the time
//!   `_try_worktree_add` runs, the script has already `cd`'d into the main
//!   workspace and captured it once as `$WORKTREE_REPO_ROOT` — asking git
//!   again for the same answer would only add a subprocess and, for a unit
//!   test, an implicit dependency on the test process's own cwd. The value is
//!   identical; only how it is obtained changed.
//! - The first branch (path could not be parsed) reports `conflict_path` —
//!   the **raw**, unresolved text extracted from git's error — never the
//!   `cd … && pwd`-resolved form; so does the "different worktree" branch.
//!   Only the uncommitted-changes and auto-switch branches use the resolved
//!   path, matching the shell's own `$conflict_path` vs. `$abs_conflict`
//!   split.
//! - `cd "$dir" 2>/dev/null && pwd || abs="$dir"`: a candidate that is not (or
//!   is no longer) a directory falls back to its literal spelling rather than
//!   erroring, and — see [`resolve_dir_or_literal`] — one that IS a directory
//!   is reported by its **logical** spelling, not resolved through any
//!   symlink in it.
//! - `git checkout` and `git status --porcelain` run in the conflicting
//!   worktree via `-C`, with stderr discarded exactly as the shell's
//!   `2>/dev/null` did.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Everything the guard needs, once assembled by the shell wrapper.
pub struct Options {
    /// `git worktree add`'s captured stderr, verbatim (read from stdin by the
    /// CLI layer — this can be arbitrarily long and is not argv-safe).
    pub error_output: String,
    /// The branch `git worktree add` was trying to create or attach.
    pub branch: String,
    /// The repo's already-resolved default branch — the recovery target.
    pub default_branch: String,
    /// `$ISSUE_NUMBER`, quoted into two of the five message bodies.
    pub issue: String,
    /// The main workspace root (`$WORKTREE_REPO_ROOT` — see the module docs
    /// for why this is passed in rather than re-derived).
    pub repo_root: PathBuf,
    /// Print nothing at all. See the module docs: this gates every message,
    /// including the ones that would otherwise be errors.
    pub quiet: bool,
}

/// Run the guard. Returns the process exit code — see the module docs for
/// what each of 0/1/2 means.
pub fn run(opts: &Options) -> i32 {
    let out = Reporter { quiet: opts.quiet };

    // `echo "$error_output" | grep -q "is already used by worktree at"` — a
    // plain substring test, deliberately looser than the quoted-path pattern
    // extracted next: this rung only asks "is it worth parsing at all".
    if !opts.error_output.contains("is already used by worktree at") {
        return 1;
    }

    let Some(conflict_path) = extract_conflict_path(&opts.error_output) else {
        out.error(&format!(
            "Cannot create worktree: branch '{}' is already checked out in another worktree.",
            opts.branch
        ));
        out.plain("");
        out.plain("  The branch is in use elsewhere. To free it, find the worktree with:");
        out.plain("    git worktree list");
        out.plain(&format!("  Then switch that worktree to {}:", opts.default_branch));
        out.plain(&format!("    cd <worktree-path> && git checkout {}", opts.default_branch));
        return 0;
    };

    let abs_conflict = resolve_dir_or_literal(Path::new(&conflict_path));
    let abs_main = resolve_dir_or_literal(&opts.repo_root);

    if abs_conflict != abs_main {
        out.error(&format!("Cannot create worktree for branch '{}':", opts.branch));
        out.plain(&format!("  Branch is already checked out at: {conflict_path}"));
        out.plain("");
        out.plain("  To fix:");
        out.plain(&format!("    cd {conflict_path} && git checkout {}", opts.default_branch));
        return 0;
    }

    // `git -C "$abs_conflict" status --porcelain 2>/dev/null`
    let uncommitted = git_stdout(&abs_conflict, &["status", "--porcelain"]).unwrap_or_default();
    if !uncommitted.is_empty() {
        out.error(&format!(
            "Cannot create worktree for issue #{}: branch '{}'",
            opts.issue, opts.branch
        ));
        out.plain(&format!(
            "  is already checked out at '{}' (main worktree).",
            abs_conflict.display()
        ));
        out.plain("");
        out.plain("  The main worktree has uncommitted changes — cannot auto-switch.");
        out.plain("  To fix manually:");
        out.plain(&format!("    cd {}", abs_conflict.display()));
        out.plain("    git stash  # or commit your changes");
        out.plain(&format!("    git checkout {}", opts.default_branch));
        out.plain(&format!("  Then rerun: ./.loom/scripts/worktree.sh {}", opts.issue));
        return 0;
    }

    out.warning(&format!("Branch '{}' is checked out in the main worktree.", opts.branch));
    out.info(&format!(
        "Main worktree is clean — auto-switching to {} branch...",
        opts.default_branch
    ));

    // `git -C "$abs_conflict" checkout "$DEFAULT_BRANCH" 2>/dev/null`
    if git_status_code(&abs_conflict, &["checkout", &opts.default_branch]) == 0 {
        out.success(&format!("Main worktree switched to {} branch", opts.default_branch));
        return 2;
    }

    out.error(&format!("Failed to switch main worktree to {} branch.", opts.default_branch));
    out.plain("  To fix manually:");
    out.plain(&format!(
        "    cd {} && git checkout {}",
        abs_conflict.display(),
        opts.default_branch
    ));
    out.plain(&format!("  Then rerun: ./.loom/scripts/worktree.sh {}", opts.issue));
    0
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// `grep -o "is already used by worktree at '[^']*'" | sed "s/is already used
/// by worktree at '//;s/'$//"`.
///
/// Returns every match's quoted content, joined with `\n` — matching what a
/// multi-line `$(…)` command substitution over `grep -o`'s one-match-per-line
/// output would produce. `None` when there is no quoted match at all (the
/// shell's empty-string case).
fn extract_conflict_path(error_output: &str) -> Option<String> {
    const PREFIX: &str = "is already used by worktree at '";
    let mut matches = Vec::new();
    let mut rest = error_output;
    while let Some(start) = rest.find(PREFIX) {
        let after_prefix = &rest[start + PREFIX.len()..];
        let Some(end) = after_prefix.find('\'') else {
            break;
        };
        matches.push(after_prefix[..end].to_string());
        rest = &after_prefix[end + 1..];
    }
    if matches.is_empty() {
        None
    } else {
        Some(matches.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `abs=$(cd "$path" 2>/dev/null && pwd) || abs="$path"`.
///
/// Bash's `cd`/`pwd` here run in **logical** mode — the default, with no
/// `-P` anywhere in the retired function — which does **not** resolve
/// symbolic links; `pwd` simply reports `$PWD` as `cd` last set it, and a
/// `cd` into an absolute path with no `..` component sets `$PWD` to that
/// argument verbatim. [`std::fs::canonicalize`] would be the wrong tool here
/// precisely because it always resolves them: a `repo_root` reached through
/// a symlinked mount (the shape the #7849 `logical_cwd` comment on
/// [`super::cleanup`] documents for the same reason) would then compare equal
/// to a physically-identical but differently-spelled `conflict_path` when the
/// retired shell would have compared them as different worktrees. Preserving
/// that — even though it reads as "wrong" in the abstract — is the point: a
/// port's job is to agree with what shipped, not to opportunistically fix it
/// along the way.
///
/// Only the existence check is physical (`is_dir`, which does follow
/// symlinks, matching `[[ -d ]]`/`cd`'s own requirement that the target
/// actually be enterable); the returned spelling is not.
fn resolve_dir_or_literal(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Every real caller passes an already-absolute string (git's own
        // error text, or `$WORKTREE_REPO_ROOT`'s `$(pwd)` answer); this is a
        // defensive fallback for a corpus/test value that is not, mirroring
        // `cd`'s own relative-to-cwd resolution.
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    if absolute.is_dir() {
        absolute
    } else {
        path.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// `$(git -C "$dir" … 2>/dev/null)` — trimmed stdout on success, `None` when
/// git exits non-zero or could not run.
fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

/// `git -C "$dir" … >/dev/null 2>&1; echo $?` — a git that cannot even be
/// spawned answers 127, matching bash's own report for that failure.
fn git_status_code(dir: &Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(127, |s| s.code().unwrap_or(127))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// All four message levels, each gated on `--quiet` — including `error`,
/// unlike [`super::wip::Out::error`]. See the module docs for why: the
/// retired shell function wrapped its whole body, `print_error` included, in
/// one `if [[ "$JSON_OUTPUT" != "true" ]]`.
struct Reporter {
    quiet: bool,
}

const RED: &str = "\x1b[0;31m";
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[1;33m";
const BLUE: &str = "\x1b[0;34m";
const NC: &str = "\x1b[0m";

impl Reporter {
    /// `print_error` — stderr, but only when not `--quiet` (see the struct
    /// doc for why this one differs from [`super::wip::Out::error`]).
    fn error(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{RED}ERROR: {msg}{NC}");
        }
    }

    /// A bare `echo` line — stdout, no color, no icon.
    fn plain(&self, msg: &str) {
        if !self.quiet {
            println!("{msg}");
        }
    }

    fn info(&self, msg: &str) {
        if !self.quiet {
            println!("{BLUE}ℹ {msg}{NC}");
        }
    }

    fn success(&self, msg: &str) {
        if !self.quiet {
            println!("{GREEN}✓ {msg}{NC}");
        }
    }

    fn warning(&self, msg: &str) {
        if !self.quiet {
            println!("{YELLOW}⚠ {msg}{NC}");
        }
    }
}

#[cfg(test)]
mod tests;
