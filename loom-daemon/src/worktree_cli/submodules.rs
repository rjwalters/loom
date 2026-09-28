//! `worktree.sh`'s submodule initialization (#8195 slice 8, epic #7810) — the
//! post-`git worktree add` step that populates a fresh worktree's
//! uninitialized submodules, borrowing the main workspace's already-cloned
//! objects instead of re-fetching them.
//!
//! # What moved here
//!
//! One block, in the shell's own order:
//!
//! 1. `git submodule status` in the new worktree; the lines beginning `-`
//!    (never initialized) are the work list.
//! 2. If the list is empty, nothing is printed and nothing runs.
//! 3. Otherwise `ℹ Initializing N submodule(s) with shared objects...`, then
//!    one `git submodule update --init --recursive` per entry, each under a
//!    deadline (`LOOM_SUBMODULE_TIMEOUT`, default 300s), with
//!    `--reference <main-workspace>/modules/<path>` when that object store
//!    exists.
//! 4. One summary line: `✓ Submodules initialized with shared objects`, or
//!    the three-line `⚠`/`ℹ`/`ℹ` warning if any entry failed.
//!
//! # Why this slice
//!
//! It is fifty lines of shell holding four distinct defects, none of which a
//! linter can see and none of which change what the script *prints* — so
//! every one of them survived six months of fix commits on the file around
//! them.
//!
//! **1. `awk '{print $2}'` on a path (#7858's class).** The work list was
//! extracted with `git submodule status | grep '^-' | awk '{print $2}'`. A
//! submodule whose path contains a space is truncated at the space, and the
//! truncated string is then used for *both* halves of the operation: it is
//! interpolated into the `--reference` directory and passed to git as the
//! pathspec after `--`. The best case is that git matches nothing and the
//! step reports a failure it did not cause; there is no case where it does
//! the right thing. This is the same construct family as #7858, where a
//! whitespace-split path turned an orphan guard into an `rm -rf` on a live
//! worktree. Here the parse is structural: `-` lines are the only ones git
//! emits *without* a trailing ` (describe)` suffix, so everything after the
//! object id on such a line is the path, whole, spaces and all — see
//! [`parse_uninitialized`].
//!
//! **2. `timeout` is not on a stock macOS.** `timeout(1)` is GNU coreutils.
//! On a Mac without Homebrew coreutils on `PATH` — a supported Loom host —
//! every iteration of the loop failed with `command not found`, so the
//! worktree got NO submodules and the operator got the generic "Some
//! submodules failed to initialize" line with the real reason buried in
//! stderr. The deadline is now [`crate::proc_exec::run_bounded`], which is in
//! the binary and also terminates the child's whole process **group** —
//! `timeout` without `--kill-after`/`--foreground` leaves a wedged
//! `git fetch` descendant behind.
//!
//! **3. The `--reference` fast path never fired.** `MAIN_GIT_DIR` came from
//! `git rev-parse --git-common-dir`, which at a repo root answers with the
//! RELATIVE `.git`. The `[[ -d "$MAIN_GIT_DIR/modules/$submod_path" ]]` test
//! that consumed it, however, ran after a `cd "$ABS_WORKTREE_PATH"` — and in
//! a worktree `.git` is a *file*, so the test was false every time. Object
//! sharing, the entire stated point of the block ("much faster than
//! downloading from network and saves disk space"), was dead code on every
//! host, while the success line kept claiming "with shared objects". This is
//! the one place the port deliberately changes behaviour rather than
//! preserving it: [`git_common_dir`] resolves the answer against the repo
//! root, so a relative reply becomes an absolute path and the `--reference`
//! arm is reachable for the first time. Argued at length under "The one
//! divergence" below.
//!
//! **4. A `$$`-keyed failure flag in `/tmp`.** The work loop is the tail of a
//! pipeline, so it ran in a subshell and could not set a variable the parent
//! would see; the shell signalled failure by writing `/tmp/loom-submodule-
//! status-$$` and testing for the file afterwards. That is a predictable path
//! in a world-writable directory: a pre-planted file makes a wholly
//! successful run report failure, and a pre-planted *symlink* makes the
//! `echo` write through it as the agent's user. It is also PID-keyed, and
//! pids recycle. A `bool` has none of those properties.
//!
//! Two smaller things go with them. The block `cd`-ed the script's process
//! into the worktree and `cd -`-ed back, so any early exit inside it left the
//! rest of the create path running from the wrong directory; nothing here
//! changes the caller's cwd. And `git submodule status` was run twice — once
//! to count, once to list — so a submodule initialized by a concurrent
//! process in between made the printed count disagree with the work actually
//! done; it is run once here and both the count and the list come from those
//! same bytes.
//!
//! # Exit code
//!
//! **Always 0.** Argued, not defaulted, and for the same reason as
//! [`super::link`]: this is best-effort provisioning that runs *after* the
//! worktree exists. The shell's own failure path was `print_warning "Some
//! submodules failed to initialize (worktree still created)"` — a warning,
//! never a non-zero status — and the call site is inside a `set -e` script
//! that has already created the thing a non-zero code would tell it to
//! abandon. There is no answer a caller branches on, so there is no code to
//! reserve for one. The caller keeps its own `|| true` regardless, so the two
//! agree even if this ever changes.
//!
//! # The one divergence
//!
//! `--reference` now actually gets passed (defect 3 above). Everything
//! observable is unchanged — the same lines, in the same order, with the same
//! counts, and the same always-0 exit — but the submodules are now populated
//! from the main workspace's object store instead of over the network, which
//! is what the code was written to do and what its message has always said it
//! did.
//!
//! This is recorded as a divergence rather than smuggled in as a "port"
//! because it has a failure mode of its own worth naming: `--reference`
//! records the borrowed store in the submodule's `objects/info/alternates`,
//! so deleting the main workspace's `modules/<path>` afterwards breaks the
//! borrowing clone. That risk was accepted when the block was written (#3274)
//! and is unchanged in kind; what changes is that it is now real. The
//! alternative — freezing a dead optimization into Rust so the port could
//! claim byte-identical behaviour — would make the port a worse artifact than
//! the shell it replaces, and would have to be undone by a follow-up issue
//! that could only re-derive this same reasoning.
//!
//! # Where the evidence is
//!
//! `tests/worktree_submodules_differential.rs` replays a shared corpus
//! through both this module and `tests/fixtures/worktree-submodules-retired.sh`
//! — a frozen copy of the retired block — and compares stdout, exit code and
//! the resulting on-disk submodule state. The space-bearing case is pinned
//! there as a known divergence with this side named as correct, as is the
//! `--reference` borrow. The unit tests below cover the parse grammar (every
//! path shape a shell fixture cannot express: consecutive spaces, a
//! describe-shaped tail, non-UTF-8 bytes) and the absolute resolution of the
//! reference path, neither of which the differential can reach through
//! black-box output alone.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::proc_exec::{self, Completion};

use super::wip::Out;

/// `LOOM_SUBMODULE_TIMEOUT`'s default, in seconds.
///
/// Generous on purpose, and inherited verbatim: a cold clone of a large
/// reference corpus with no object cache legitimately exceeds thirty seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// What to initialize, and where from.
pub struct Options {
    /// The main workspace root (`git rev-parse --show-toplevel`), whose
    /// `modules/` object stores are the `--reference` sources.
    ///
    /// Passed in rather than re-derived for the reason [`super::link`] states:
    /// by this point the create path has already auto-navigated out of any
    /// worktree, and this is the answer the rest of it used.
    pub repo_root: PathBuf,
    /// Absolute path of the worktree that was just created.
    pub worktree: PathBuf,
    /// Print nothing at all.
    ///
    /// Mirrors the shell's `if [[ "$JSON_OUTPUT" != "true" ]]` guard around
    /// every one of these lines. Note it does NOT suppress the child git's own
    /// output: the retired block deliberately left `git submodule update`'s
    /// stderr unredirected so the underlying error is visible, and that was
    /// true in `--json` mode too (where the script had already pointed fd 1 at
    /// stderr).
    pub quiet: bool,
    /// Per-submodule deadline. `LOOM_SUBMODULE_TIMEOUT` seconds.
    pub timeout: Duration,
}

/// Initialize every uninitialized submodule. See the module docs; always 0.
#[must_use]
pub fn run(opts: &Options) -> i32 {
    let reporter = Reporter {
        out: Out::new(false),
        quiet: opts.quiet,
    };

    let pending = parse_uninitialized(&submodule_status(&opts.worktree));
    if pending.is_empty() {
        // The shell's `if [[ "$UNINIT_SUBMODULES" -gt 0 ]]`: silent, not even
        // an "up to date" line. Most repos have no submodules at all and this
        // runs on every worktree creation.
        return 0;
    }

    reporter.info(&format!("Initializing {} submodule(s) with shared objects...", pending.len()));

    let common_dir = git_common_dir(&opts.repo_root);
    let mut any_failed = false;
    for submodule in &pending {
        let reference = common_dir
            .as_deref()
            .map(|dir| reference_dir(dir, submodule))
            .filter(|dir| is_dir(dir));
        if !update_one(opts, submodule, reference.as_deref()) {
            // No early return: the shell's `while read` loop ran every entry
            // and only summarised at the end, so one unreachable submodule
            // never stopped the others from being populated.
            any_failed = true;
        }
    }

    if any_failed {
        reporter.warning("Some submodules failed to initialize (worktree still created)");
        reporter.info("See stderr above for the underlying git error.");
        reporter.info("You may need to run: git submodule update --init --recursive");
    } else {
        reporter.success("Submodules initialized with shared objects");
    }

    0
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// [`Out`] plus the shell's all-or-nothing `--json` suppression. Same shape as
/// [`super::link`]'s, and deliberately not shared with it: the two slices
/// suppress for the same reason but are wired by separate call sites, and a
/// shared helper would make a change to one silently retune the other.
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

// ---------------------------------------------------------------------------
// The work list
// ---------------------------------------------------------------------------

/// `cd "$worktree" && git submodule status 2>/dev/null`.
///
/// Stderr is discarded exactly as the shell's counting invocation did — a repo
/// with no `.gitmodules`, or one where the command cannot run at all, is
/// "nothing to do", not an error to report. A non-zero exit yields an empty
/// work list for the same reason.
fn submodule_status(worktree: &Path) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["submodule", "status"])
        .output();
    match out {
        Ok(out) if out.status.success() => out.stdout,
        _ => Vec::new(),
    }
}

/// The paths of submodules `git submodule status` reports as never
/// initialized, byte-exact and never split on whitespace.
///
/// The grammar (`builtin/submodule--helper.c`, `print_status`) is
/// `<state><oid> <displaypath>`, with a trailing ` (<describe>)` appended
/// **only** when the state is `' '` or `'+'`. `-` — the only state this
/// function accepts — is never described, because there is nothing checked out
/// to describe. So on a `-` line everything after the first space is the path,
/// terminator-free and unambiguous, however many spaces it contains. That is
/// why this can be a structural parse where `awk '{print $2}'` could not be,
/// and it is the whole reason the `#7858` class does not survive the port.
///
/// Returned as [`OsString`] via the raw bytes: a path is not required to be
/// UTF-8, and lossily decoding one here would hand git a pathspec that matches
/// nothing.
fn parse_uninitialized(stdout: &[u8]) -> Vec<OsString> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| line.first() == Some(&b'-'))
        .filter_map(|line| {
            // Skip the state char, then the object id up to its trailing space.
            let rest = &line[1..];
            let space = rest.iter().position(|byte| *byte == b' ')?;
            let path = &rest[space + 1..];
            if path.is_empty() {
                // A `-<oid> ` with nothing after it is not a submodule this can
                // act on. `awk` would have yielded an empty field and the shell
                // would have run `git submodule update -- ''`; refusing is the
                // only honest handling and it cannot lose real work, since git
                // never emits such a line.
                None
            } else {
                Some(OsString::from_vec(path.to_vec()))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The borrowed object store
// ---------------------------------------------------------------------------

/// The main workspace's git common dir, as an ABSOLUTE path.
///
/// `git rev-parse --git-common-dir` answers relatively when run at a repo root
/// (`.git`) and relatively-upward from a subdirectory (`../../.git`). The
/// retired shell captured that answer in the main workspace and then tested it
/// from inside the worktree, where neither spelling resolves — see "defect 3"
/// in the module docs. Anchoring it to `repo_root` here is what makes the
/// `--reference` arm reachable at all.
///
/// `None` when git cannot answer; the caller then simply never passes
/// `--reference`, which is the retired behaviour and is always safe (it costs
/// network, not correctness).
fn git_common_dir(repo_root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut bytes = out.stdout;
    while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        bytes.pop();
    }
    if bytes.is_empty() {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}

/// `"$common_dir/modules/$submodule"` — byte concatenation, NOT
/// [`Path::join`] on the submodule component.
///
/// Same reasoning as [`super::link`]'s `concat`: `join` discards the base when
/// the appended component is absolute, so an absolute-looking submodule path
/// would resolve `--reference` to somewhere outside the workspace entirely.
/// The shell could not do that, so neither does this. (`modules` is a literal
/// and is joined normally.)
fn reference_dir(common_dir: &Path, submodule: &OsStr) -> PathBuf {
    let mut bytes = common_dir.join("modules").into_os_string().into_vec();
    bytes.push(b'/');
    bytes.extend_from_slice(submodule.as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
}

/// `[[ -d "$path" ]]` — follows symlinks, as the bash operator does.
fn is_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The update itself
// ---------------------------------------------------------------------------

/// One `git submodule update --init --recursive [--reference <dir>] -- <path>`
/// under the deadline. `true` when it succeeded.
///
/// `--recursive` is inherited verbatim and is load-bearing: a top-level
/// submodule may declare submodules of its own, and without it those stay
/// empty while the command still exits 0 — a half-populated reference
/// directory with no error (#3274).
///
/// The child's stdout and stderr are relayed verbatim to this process's own.
/// The retired block let the child inherit the script's descriptors, and
/// deliberately did NOT send stderr to `/dev/null` so the real git error is
/// visible; relaying preserves both the bytes and the destination. What it
/// does not preserve is the *interleaving* — output now appears when the child
/// finishes rather than as it streams. That is the price of draining the pipes
/// (see [`crate::proc_exec`] for why an undrained child of unbounded output is
/// indistinguishable from a hang), and it buys the timeout path something the
/// shell never had: whatever the child managed to say before the deadline is
/// still printed instead of being lost with the pipe.
fn update_one(opts: &Options, submodule: &OsStr, reference: Option<&Path>) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(&opts.worktree)
        .args(["submodule", "update", "--init", "--recursive"]);
    if let Some(dir) = reference {
        cmd.arg("--reference").arg(dir);
    }
    cmd.arg("--").arg(submodule);

    match proc_exec::run_bounded(cmd, opts.timeout) {
        Ok(Completion::Exited(out)) => {
            relay(&out.stdout, &out.stderr);
            out.status.success()
        }
        Ok(Completion::TimedOut { stdout, stderr }) => {
            relay(&stdout, &stderr);
            // `timeout` exited 124 here, which the shell's `if !` read as
            // failure. Same verdict, but say so: the generic summary line
            // below cannot distinguish a deadline from a fetch error, and
            // "stderr above" is empty for a hang.
            eprintln!(
                "git submodule update for '{}' exceeded {}s (LOOM_SUBMODULE_TIMEOUT)",
                Path::new(submodule).display(),
                opts.timeout.as_secs()
            );
            false
        }
        Err(err) => {
            // The shell reached here when `timeout` itself was missing (exit
            // 127 on a stock macOS) and printed nothing but the generic
            // summary. Name it.
            eprintln!(
                "git submodule update for '{}' could not run: {err}",
                Path::new(submodule).display()
            );
            false
        }
    }
}

/// Write the child's captured bytes to our own stdout/stderr, unmodified.
fn relay(stdout: &[u8], stderr: &[u8]) {
    if !stdout.is_empty() {
        let handle = std::io::stdout();
        let mut lock = handle.lock();
        let _ = lock.write_all(stdout);
        let _ = lock.flush();
    }
    if !stderr.is_empty() {
        let handle = std::io::stderr();
        let mut lock = handle.lock();
        let _ = lock.write_all(stderr);
        let _ = lock.flush();
    }
}

#[cfg(test)]
mod tests;
