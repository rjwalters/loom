//! `worktree.sh`'s post-`git worktree add` FINALIZATION steps (#8195 slice 16,
//! epic #7810) — the three blocks that run after [`super::link`] has provisioned
//! the shared artifacts and before the create path prints its result.
//!
//! # What moved here
//!
//! In the shell's own order, which is observable because two of the three print:
//!
//! 1. **`core.hooksPath` (#3638).** `git -C <worktree> config core.hooksPath
//!    .githooks`, guarded on the main workspace actually shipping a tracked
//!    `.githooks/` directory. The guard is the whole point of #3638: git treats
//!    a *nonexistent* `hooksPath` as "no hooks at all", so pointing it at a
//!    missing directory silently disables hooks a repo configured elsewhere.
//! 2. **The per-worktree Cargo target dir (#8458).** [`provision::provision`],
//!    whose report line goes to stderr and whose directory is handed to the
//!    hook below through `LOOM_WORKTREE_CARGO_TARGET_DIR` — deliberately NOT
//!    `CARGO_TARGET_DIR`, which would make the hook's main-workspace binary
//!    lookup miss and reintroduce #6013/#6014's rebuild storm.
//! 3. **The project `post-worktree.sh` hook.** `<repo-root>/.loom/hooks/
//!    post-worktree.sh`, run from inside the new worktree with `(worktree,
//!    branch, issue)` as argv, its own stdout/stderr inherited so the project's
//!    setup output is not a black box.
//!
//! # Why this family
//!
//! It is the create path's only remaining block that **hands an arbitrary,
//! interpolated path to an external program** — `(cd "$ABS_WORKTREE_PATH" &&
//! "$POST_WORKTREE_HOOK" "$ABS_WORKTREE_PATH" "$BRANCH_NAME" "$ISSUE_NUMBER")`
//! — plus a `git -C "$ABS_WORKTREE_PATH"` and an
//! `export VAR="$("$_pwt_bin" … "$ABS_WORKTREE_PATH")"`. That is #7858's class
//! (an unquoted/word-split path turning a guard into an `rm -rf` on a live
//! worktree): every one of those is a *word-split-able string* in bash and an
//! `OsString` that [`std::process::Command`] takes whole here, so the class is
//! gone by construction rather than by review. `tests::hook_receives_argv_with_
//! spaces_intact` is the regression that fails if it ever comes back.
//!
//! # Which binary provisions the target dir — a deliberate change (#8458)
//!
//! The retired block resolved its own binary with `loom_locate_daemon_bin`
//! (*"the daemon this caller manages or probes"*) while every other subcommand
//! on the create path goes through `$_WT_DAEMON_BIN` /
//! `loom_resolve_self_daemon_bin` (*"the binary that IMPLEMENTS this script"*) —
//! the two tiers `lib/locate-daemon-bin.sh`'s own header draws a line between.
//! For a ported subcommand the second is the correct tier, and after this slice
//! the question does not arise at all: the provisioning runs **in-process**, in
//! the same binary the call site already resolved. One resolution, not two.
//!
//! # Exit 0, always
//!
//! Matching its two neighbours ([`super::link`], [`super::submodules`]) and for
//! the same reason: by the time any of this runs `git worktree add` has already
//! succeeded and the [`super::sentinel`] is written, so the caller has a usable
//! worktree it must not be told to abandon over a build-cache optimisation or a
//! project hook.
//!
//! One consequence is argued rather than inherited. The retired `git -C …
//! config core.hooksPath` ran bare under `set -e`, so a failure aborted the
//! script *after* the worktree existed — no symlinks, no hook, no success
//! message, and (in `--json` mode) no document at all for a caller that had
//! already been handed a real worktree. That is not a contract worth
//! preserving: the port **warns and continues**. Nothing observable changes on
//! the path that can actually be reached, because a `git config` write into a
//! worktree git has just created does not fail.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::worktree_ops::cargo_target::provision::{self, Provision};

use super::wip::Out;

/// The hook's path, relative to the main workspace root.
///
/// Repo-owned by convention (`.loom/hooks/` is outside Loom's uninstall
/// ownership boundary — #5971), so it is looked up and executed, never
/// written.
pub const HOOK_REL_PATH: &str = ".loom/hooks/post-worktree.sh";

/// The tracked hooks directory whose presence gates `core.hooksPath` (#3638).
pub const GITHOOKS_DIR: &str = ".githooks";

/// The value `core.hooksPath` is set to — a worktree-relative path, exactly as
/// the shell wrote it, so each worktree resolves it against its own root.
pub const HOOKS_PATH_VALUE: &str = ".githooks";

/// The variable the hook reads. **Not** `CARGO_TARGET_DIR`: see the module doc.
pub const CARGO_TARGET_ENV: &str = "LOOM_WORKTREE_CARGO_TARGET_DIR";

/// What to finalize, and where.
pub struct Options {
    /// The main workspace root (`git rev-parse --show-toplevel`), resolved by
    /// the caller and passed in rather than re-derived — by this point the
    /// create path has already auto-navigated out of any worktree, and this is
    /// the answer the rest of it used. Same argument as [`super::link`]'s.
    pub repo_root: PathBuf,
    /// Absolute path of the worktree that was just created.
    pub worktree: PathBuf,
    /// The branch checked out in it — the hook's `$2`.
    pub branch: String,
    /// The issue number — the hook's `$3`. A string, not a number: the shell
    /// interpolated `$ISSUE_NUMBER` verbatim (leading zeros included) and the
    /// hook is an arbitrary project script that may care.
    pub issue: String,
    /// Print nothing at all.
    ///
    /// Mirrors the shell's `if [[ "$JSON_OUTPUT" != "true" ]]` guard around
    /// every one of these lines — NOT `Out`'s stderr routing, for the reason
    /// [`super::link`] states. It does **not** silence the hook's own output,
    /// which the retired block left inherited so a project's setup failure is
    /// visible; nor the [`Provision`] report line, whose stderr the retired
    /// block deliberately did not swallow (it is the one operator-visible sign
    /// the #8458 scheme is on).
    pub quiet: bool,
}

/// Run every finalization step, in the shell's order. See the module docs for
/// why this cannot fail.
#[must_use]
pub fn run(opts: &Options) -> i32 {
    let out = Reporter {
        out: Out::new(false),
        quiet: opts.quiet,
    };

    configure_hooks_path(opts, &out);
    let target_dir = provision_cargo_target_dir(opts);
    run_post_worktree_hook(opts, target_dir.as_deref(), &out);

    0
}

// ---------------------------------------------------------------------------
// 1. core.hooksPath (#3638)
// ---------------------------------------------------------------------------

/// `if [[ -d "$WORKTREE_REPO_ROOT/.githooks" ]]; then git -C … config … ; fi`
///
/// The `[[ -d ]]` follows symlinks, so [`Path::is_dir`] is the exact
/// counterpart (it stats through links; `symlink_metadata` would not).
fn configure_hooks_path(opts: &Options, out: &Reporter) {
    if !opts.repo_root.join(GITHOOKS_DIR).is_dir() {
        return;
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&opts.worktree)
        .args(["config", "core.hooksPath", HOOKS_PATH_VALUE])
        .status();
    let ok = matches!(&status, Ok(s) if s.success());
    if !ok {
        // Argued in the module doc: the retired line's `set -e` abort is not a
        // contract worth preserving. Warn — never fail the worktree.
        out.warning(&format!(
            "Could not set core.hooksPath in {} (worktree still created)",
            opts.worktree.display()
        ));
    }
}

// ---------------------------------------------------------------------------
// 2. The per-worktree Cargo target dir (#8458)
// ---------------------------------------------------------------------------

/// `_pwt_bin cargo-target-dir provision --repo-root … --report <worktree>`.
///
/// Returns the directory to export, or `None` for every not-applicable case —
/// the same "empty stdout means no directory" answer the subcommand gives.
fn provision_cargo_target_dir(opts: &Options) -> Option<PathBuf> {
    let outcome: Provision = provision::provision(&opts.repo_root, &opts.worktree);
    // `--report`'s destination, verbatim: stderr, two leading spaces, and NOT
    // gated on `--quiet`. In `--json` mode the shell had already pointed fd 1
    // at stderr, so this line landed on stderr in both modes before the port
    // too.
    if let Some(line) = outcome.report_line() {
        eprintln!("  {line}");
    }
    outcome.dir().map(Path::to_path_buf)
}

// ---------------------------------------------------------------------------
// 3. The project post-worktree hook
// ---------------------------------------------------------------------------

/// `if [[ -x "$POST_WORKTREE_HOOK" ]]; then … fi`, with the hook run from
/// inside the worktree.
fn run_post_worktree_hook(opts: &Options, target_dir: Option<&Path>, out: &Reporter) {
    let hook = opts.repo_root.join(HOOK_REL_PATH);
    if !is_executable(&hook) {
        return;
    }

    out.info("Running project-specific post-worktree hook...");

    let mut cmd = Command::new(&hook);
    cmd.current_dir(&opts.worktree)
        // argv verbatim, as three separate OsString arguments. The shell passed
        // the same three quoted; in Rust there is no word-splitting stage for a
        // space-bearing worktree path to survive.
        .arg(&opts.worktree)
        .arg(&opts.branch)
        .arg(&opts.issue);
    // The shell exported this unconditionally once it had a binary to ask —
    // including as the empty string, which every consumer tests with `-n`. We
    // ARE that binary, so the export is unconditional here.
    cmd.env(
        CARGO_TARGET_ENV,
        target_dir.map_or_else(OsString::new, |d| d.as_os_str().to_os_string()),
    );

    match cmd.status() {
        Ok(status) if status.success() => out.success("Post-worktree hook completed"),
        // A non-zero exit AND a hook that could not be spawned at all collapse
        // into one warning, exactly as the shell's `if (cd … && "$HOOK" …)`
        // did: an exec failure there is a non-zero subshell too.
        _ => out.warning("Post-worktree hook failed (worktree still created)"),
    }
}

/// `[[ -x <path> ]]` — exists, is not a directory, and carries an execute bit.
///
/// The shell's `-x` follows symlinks and is true only for something the
/// current user may execute; a directory is `-x` to bash but is not something
/// `Command` can run, so it is excluded here. That divergence can only turn a
/// guaranteed spawn failure into a skip.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// [`Out`] plus the shell's all-or-nothing `--json` suppression. Same shape as
/// [`super::link`]'s and [`super::submodules`]', and deliberately not shared
/// with either, for the reason [`super::submodules`] gives: the three slices
/// suppress for the same reason but are wired by separate call sites, and a
/// shared helper would make a change to one silently retune the others.
struct Reporter {
    out: Out,
    quiet: bool,
}

impl Reporter {
    fn info(&self, msg: &str) {
        if !self.quiet {
            self.out.info(msg);
        }
    }
    fn success(&self, msg: &str) {
        if !self.quiet {
            self.out.success(msg);
        }
    }
    fn warning(&self, msg: &str) {
        if !self.quiet {
            self.out.warning(msg);
        }
    }
}

#[cfg(test)]
mod tests;
