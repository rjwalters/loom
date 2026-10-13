//! The git half of the checkout fast-forward (#10869): every command it runs
//! in a registered workspace's main checkout.
//!
//! Two commands here write to the checkout: [`fast_forward`], a
//! `merge --ff-only` with hooks off, and [`discard_identical`], a
//! `checkout HEAD -- <paths>` with hooks off and literal pathspecs that is
//! only ever run on paths proven identical to `origin/<branch>`, immediately before the merge
//! (#11553). Everything else is a read, a `git ls-remote` that writes
//! nothing, or a `fetch` that moves only the remote-tracking ref (not even
//! `FETCH_HEAD`). Nothing here runs `reset`, `rebase`, `stash` or `clean`,
//! makes a merge commit, or removes a lock file.
//!
//! Two commands use the network: [`remote_head`] and [`fetch`]. Every ref
//! operand that comes from the remote sits behind a standalone `--` (#9106,
//! #9479), and the default branch's name is checked with
//! [`crate::refname::check_refname`] before it is used at all.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::cmd_out::CmdOutcome;
use crate::fleet_sync::git_spawn_log::{memory_config_for, run_logged};

/// Bound for `git ls-remote` of one ref: a connection and a ref advertisement.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound for the fetch of one branch.
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

pub(super) fn tracking(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

/// Run `git` in `root` to an exit. `config` is `-c` pairs placed before the
/// subcommand. `Err` when it could not be run or outlived `timeout`.
pub(super) fn run(
    root: &Path,
    config: &[&str],
    args: &[&str],
    timeout: Duration,
) -> Result<Output, String> {
    run_with_index(root, config, args, timeout, None)
}

/// [`run`], with `GIT_INDEX_FILE` set to `index` when it is `Some`.
fn run_with_index(
    root: &Path,
    config: &[&str],
    args: &[&str],
    timeout: Duration,
    index: Option<&Path>,
) -> Result<Output, String> {
    let mut cmd = command(root, config, args);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    let result = run_logged("checkout_ff", root, cmd, timeout);
    match crate::cmd_out::outcome_of(result, timeout) {
        CmdOutcome::Ran(out) => Ok(out),
        CmdOutcome::Unavailable(why) => Err(format!("git {}: {why}", args.join(" "))),
    }
}

/// The `git` command [`run`] spawns, before credentials. `fetch` always
/// carries the memory bound (#11059).
fn command(root: &Path, config: &[&str], args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    for pair in memory_config_for(args.first().copied().unwrap_or_default())
        .iter()
        .chain(config)
    {
        cmd.arg("-c").arg(pair);
    }
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        // Loom's own ref moves are not external changes (#11566).
        .env("GIT_REFLOG_ACTION", super::external_change::tag("ff"))
        // Refusals are read from stderr below, so the wording must be git's own.
        .env("LC_ALL", "C");
    // An inherited repository override would aim every command at some other
    // repository than `root`.
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    cmd
}

pub(super) fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

pub(super) fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// [`run`], requiring exit 0. Returns trimmed stdout.
pub(super) fn ok(root: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let out = run(root, &[], args, timeout)?;
    if out.status.success() {
        Ok(stdout(&out))
    } else {
        Err(format!("git {} failed: {}", args.join(" "), stderr(&out)))
    }
}

/// The default branch's name, from the clone's own `origin/HEAD`. Local: no
/// remote is asked, and `main` is never guessed. `None` when `origin/HEAD` is
/// unset, or names a branch git could read as an option (#9106).
pub(super) fn default_branch(root: &Path, timeout: Duration) -> Option<String> {
    let args = [
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ];
    let name = ok(root, &args, timeout).ok()?;
    name.strip_prefix("origin/")
        .filter(|b| crate::refname::check_refname(b).is_ok())
        .map(str::to_string)
}

/// The commit `refs/heads/<branch>` is at on the remote, by `git ls-remote`:
/// one connection, no objects, nothing written in the clone.
pub(super) fn remote_head(root: &Path, branch: &str, timeout: Duration) -> Result<String, String> {
    let name = format!("refs/heads/{branch}");
    let listed = ok(root, &["ls-remote", "--quiet", "origin", "--", &name], timeout)?;
    listed
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(sha, at)| *at == name && sha.len() >= 40)
        .map(|(sha, _)| sha.to_string())
        .ok_or_else(|| format!("origin has no {name}"))
}

/// The commit the clone's `origin/<branch>` is at; `None` when it has never
/// been fetched. Local.
pub(super) fn tracking_head(root: &Path, branch: &str, timeout: Duration) -> Option<String> {
    let spec = format!("{}^{{commit}}", tracking(branch));
    ok(root, &["rev-parse", "--verify", "--quiet", &spec], timeout)
        .ok()
        .filter(|sha| !sha.is_empty())
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
    // option parsing before the ref operand (#9106). `--no-write-fetch-head`:
    // this is the operator's checkout, and its `FETCH_HEAD` is theirs.
    let refspec = format!("+refs/heads/{branch}:{}", tracking(branch));
    ok(
        root,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
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

/// The tracked dirty paths, exact: `status --porcelain -z` (NUL-separated,
/// never C-quoted) parsed by
/// [`crate::git_ff_identical::tracked_paths_from_porcelain_z`]. `Err` when
/// the output cannot be parsed into exact paths (malformed, or not UTF-8).
pub(super) fn tracked_paths(root: &Path, timeout: Duration) -> Result<Vec<String>, String> {
    let args = crate::git_ff_identical::STATUS_ARGS;
    let out = run(root, &[], &args, timeout)?;
    if !out.status.success() {
        return Err(format!("git {} failed: {}", args.join(" "), stderr(&out)));
    }
    crate::git_ff_identical::tracked_paths_from_porcelain_z(&out.stdout)
        .ok_or_else(|| "git status -z: a path that is not exact UTF-8".to_string())
}

/// A private copy of a checkout's index, for probes that would otherwise
/// rewrite the real one. Removed on drop.
pub(super) struct ScratchIndex {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

/// Copy the checkout's index to a fresh temp dir. `git diff <commit> --
/// <path>` refreshes the stat cache and writes the index back whenever it
/// meets a file that is stat-dirty but identical, and `--no-optional-locks`
/// does not stop it (git's `refresh_index_quietly`). A probe that runs
/// against this copy leaves the operator's index and its lock alone.
pub(super) fn scratch_index(root: &Path, timeout: Duration) -> Result<ScratchIndex, String> {
    let at = ok(root, &["rev-parse", "--git-path", "index"], timeout)?;
    let real = root.join(at);
    let dir = tempfile::TempDir::new().map_err(|e| format!("scratch index: {e}"))?;
    let path = dir.path().join("index");
    let copy_err = |e: std::io::Error| format!("scratch index from {}: {e}", real.display());
    // One handle for both the bytes and the mtime: a second `metadata(&real)`
    // could see a newer index renamed into place after the copy, and pair the
    // old contents with the new mtime.
    let mut src = std::fs::File::open(&real).map_err(copy_err)?;
    // Keep the index file's own mtime: git's racy-entry check compares each
    // entry's mtime with it, and a newer copy would make git trust a stale
    // stat entry for a file edited in the same second the index was written.
    let mtime = src
        .metadata()
        .and_then(|m| m.modified())
        .map_err(copy_err)?;
    let mut dst = std::fs::File::create(&path).map_err(copy_err)?;
    std::io::copy(&mut src, &mut dst).map_err(copy_err)?;
    dst.set_modified(mtime).map_err(copy_err)?;
    Ok(ScratchIndex { _dir: dir, path })
}

/// A bounded probe against `index`: its stdout only when `git <args>` ran
/// and exited 0. A timeout or an error answers `None`, so a check built on it
/// fails closed. `core.splitIndex=false`, so a refresh written to the copy
/// never puts a shared-index file in the real git dir.
pub(super) fn quiet_out(
    root: &Path,
    index: &ScratchIndex,
    args: &[&str],
    timeout: Duration,
) -> Option<Vec<u8>> {
    let config = ["core.splitIndex=false"];
    let out = run_with_index(root, &config, args, timeout, Some(&index.path)).ok()?;
    out.status.success().then_some(out.stdout)
}

/// The second write (#11553): `git --literal-pathspecs checkout HEAD --
/// <paths>` ([`crate::git_ff_identical::discard_args`]), discarding
/// local edits that are byte-identical to `origin/<branch>`. Only ever called
/// with paths [`crate::git_ff_identical::all_dirty_tracked_identical_to_origin`]
/// vouched for, and only immediately before [`fast_forward`].
///
/// Hooks are off (a path checkout still fires `post-checkout`), and so is
/// `submodule.recurse`, as for [`fast_forward`]. Bounded like the merge, and
/// for the same reason.
pub(super) fn discard_identical(root: &Path, paths: &[String]) -> Result<(), String> {
    let args = crate::git_ff_identical::discard_args(paths);
    let out = run(
        root,
        &["core.hooksPath=/dev/null", "submodule.recurse=false"],
        &args,
        MERGE_TIMEOUT,
    )?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("git checkout HEAD -- failed: {}", stderr(&out)))
    }
}

/// The main write: fast-forward the checked-out branch to `origin/<branch>`.
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

#[cfg(test)]
mod memory_bound_tests {
    use super::*;
    use crate::fleet_sync::git_spawn_log::{argv_of, MEMORY_BOUND_CONFIG};

    #[test]
    fn fetch_carries_the_memory_bound_and_reads_do_not() {
        let fetch = argv_of(&command(Path::new("/r"), &[], &["fetch", "origin"]));
        for pair in MEMORY_BOUND_CONFIG {
            let at = fetch.iter().position(|a| a == pair).expect(pair);
            assert_eq!(fetch[at - 1], "-c");
            assert!(at < fetch.iter().position(|a| a == "fetch").unwrap());
        }
        let read = argv_of(&command(Path::new("/r"), &["x=y"], &["rev-parse", "HEAD"]));
        assert!(read.contains(&"x=y".to_string()));
        assert!(!read.iter().any(|a| a.starts_with("pack.")));
    }
}
