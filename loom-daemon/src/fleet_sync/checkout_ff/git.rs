//! The git half of the checkout fast-forward (#10869): every command it runs
//! in a registered workspace's main checkout.
//!
//! One command here writes to the checkout: [`fast_forward`], a
//! `merge --ff-only` with hooks off. Everything else is a read, or a `fetch`
//! that moves only the remote-tracking ref. Nothing here runs `reset`,
//! `rebase`, `stash`, `checkout` or `clean`, makes a merge commit, or removes
//! a lock file.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::cmd_out::{run_command, CmdOutcome};

/// Starting bound for the fetch of one branch.
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Starting bound for each local read.
pub(super) const LOCAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound for the fast-forward itself. Long on purpose, and never shortened by
/// the startup budget: a `merge` killed halfway leaves a half-written working
/// tree and a stale `index.lock`, which is exactly the damage this step must
/// never do. It is a hang ceiling, not a performance target.
pub(super) const MERGE_TIMEOUT: Duration = Duration::from_secs(300);

/// The installed-Loom paths in a consumer tree.
const INSTALLED: &[&str] = &[".loom", ".claude/commands/loom"];

/// How many paths a detail line names before it says "and N more".
const NAMED_PATHS: usize = 5;

/// Why git did not fast-forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Refusal {
    /// A local file sits where an incoming commit writes one. Nothing changed.
    WouldOverwrite(String),
    /// Anything else git said, or a command that did not finish.
    Other(String),
}

fn tracking(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

/// Run `git` in `root` to an exit. `config` is `-c` pairs placed before the
/// subcommand. `Err` when it could not be run or outlived `timeout`.
fn run(root: &Path, config: &[&str], args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut cmd = Command::new("git");
    for pair in config {
        cmd.arg("-c").arg(pair);
    }
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        // Refusals are read from stderr below, so the wording must be git's own.
        .env("LC_ALL", "C");
    // An inherited repository override would aim every command at some other
    // repository than `root`.
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    match run_command(cmd, timeout) {
        CmdOutcome::Ran(out) => Ok(out),
        CmdOutcome::Unavailable(why) => Err(format!("git {}: {why}", args.join(" "))),
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// [`run`], requiring exit 0. Returns trimmed stdout.
fn ok(root: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let out = run(root, &[], args, timeout)?;
    if out.status.success() {
        Ok(stdout(&out))
    } else {
        Err(format!("git {} failed: {}", args.join(" "), stderr(&out)))
    }
}

/// The short commit HEAD is at; `None` when it cannot be read.
pub(super) fn head(root: &Path, timeout: Duration) -> Option<String> {
    ok(root, &["rev-parse", "--short=12", "HEAD"], timeout).ok()
}

/// The branch HEAD is on; `Ok(None)` for a detached HEAD.
pub(super) fn current_branch(root: &Path, timeout: Duration) -> Result<Option<String>, String> {
    let out = run(root, &[], &["symbolic-ref", "--quiet", "--short", "HEAD"], timeout)?;
    match out.status.code() {
        Some(0) => Ok(Some(stdout(&out)).filter(|b| !b.is_empty())),
        // `--quiet`: exit 1 and no message when HEAD is not a symbolic ref.
        Some(1) => Ok(None),
        _ => Err(format!("git symbolic-ref HEAD failed: {}", stderr(&out))),
    }
}

/// Fetch `branch` from `origin` into its remote-tracking ref.
pub(super) fn fetch(root: &Path, branch: &str, timeout: Duration) -> Result<(), String> {
    // An explicit refspec, so the remote-tracking ref moves even in a clone
    // whose configured fetch refspec does not cover this branch. `--` ends
    // option parsing before the ref operand (#9106).
    let refspec = format!("+refs/heads/{branch}:{}", tracking(branch));
    ok(
        root,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "origin",
            "--",
            &refspec,
        ],
        timeout,
    )
    .map(drop)
}

/// `(ahead, behind)`: commits only HEAD has, and commits only
/// `origin/<branch>` has.
pub(super) fn ahead_behind(
    root: &Path,
    branch: &str,
    timeout: Duration,
) -> Result<(u64, u64), String> {
    let range = format!("HEAD...{}", tracking(branch));
    let raw = ok(root, &["rev-list", "--left-right", "--count", &range], timeout)?;
    let mut counts = raw.split_whitespace().map(str::parse::<u64>);
    match (counts.next(), counts.next()) {
        (Some(Ok(ahead)), Some(Ok(behind))) => Ok((ahead, behind)),
        _ => Err(format!("git rev-list --count {range} printed `{raw}`")),
    }
}

/// Do the commits HEAD is missing from `origin/<branch>` change an installed
/// Loom file? `false` when that cannot be decided.
pub(super) fn installed_files_behind(root: &Path, branch: &str, timeout: Duration) -> bool {
    // Three dots: what `origin/<branch>` changed since the merge base, so a
    // checkout's own commits never count as "behind".
    let range = format!("HEAD...{}", tracking(branch));
    let mut args = vec!["diff", "--quiet", "--no-ext-diff", &range, "--"];
    args.extend(INSTALLED);
    run(root, &[], &args, timeout).is_ok_and(|out| out.status.code() == Some(1))
}

/// Tracked files with a staged or unstaged change, as `status` prints them.
/// Untracked files are not listed: they do not make a checkout dirty.
pub(super) fn tracked_changes(root: &Path, timeout: Duration) -> Result<Vec<String>, String> {
    // `--no-optional-locks`: a read must not rewrite the index to refresh it.
    let raw = ok(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ],
        timeout,
    )?;
    Ok(raw.lines().map(|l| l.trim().to_string()).collect())
}

/// The one write: fast-forward the checked-out branch to `origin/<branch>`.
///
/// Hooks are off (a repo's `post-merge` must not run unattended inside the
/// daemon), and so are the two config switches that would make a plain merge
/// do more than move the branch: `merge.autoStash` and `submodule.recurse`.
pub(super) fn fast_forward(root: &Path, branch: &str) -> Result<(), Refusal> {
    let out = run(
        root,
        &[
            "core.hooksPath=/dev/null",
            "merge.autoStash=false",
            "submodule.recurse=false",
        ],
        &[
            "merge",
            "--ff-only",
            // git's default lets a merge silently replace an IGNORED untracked
            // file that is in the way. This step discards nothing.
            "--no-overwrite-ignore",
            "--quiet",
            "--no-stat",
            &tracking(branch),
        ],
        MERGE_TIMEOUT,
    )
    .map_err(Refusal::Other)?;
    if out.status.success() {
        return Ok(());
    }
    Err(classify_refusal(&stderr(&out)))
}

/// Read a refused merge's stderr.
pub(super) fn classify_refusal(stderr: &str) -> Refusal {
    if !stderr.contains("would be overwritten by merge") {
        return Refusal::Other(format!("git merge --ff-only failed: {stderr}"));
    }
    // git lists the files in the way one per line, tab-indented.
    let paths: Vec<String> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix('\t'))
        .map(|p| p.trim().to_string())
        .collect();
    Refusal::WouldOverwrite(summarize(&paths))
}

/// A short list of `paths` for a one-line detail.
pub(super) fn summarize(paths: &[String]) -> String {
    let mut shown = paths
        .iter()
        .take(NAMED_PATHS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > NAMED_PATHS {
        shown.push_str(&format!(", and {} more", paths.len() - NAMED_PATHS));
    }
    shown
}
