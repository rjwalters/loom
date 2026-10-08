//! The git half of the workspace resync (#10718): everything it runs in a
//! registered workspace's clone. Reads go through the clone's object store
//! and remote-tracking refs; the only writes are a `fetch`, a throwaway
//! detached worktree under `.loom/worktrees/`, and a push that is never
//! forced. The operator's checkout (its index and working tree) is not
//! touched by anything here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::install_compat::INSTALL_METADATA_PATH;
use crate::proc_exec::{run_bounded, Completion};

/// Local plumbing: no network, small output.
const QUICK: Duration = Duration::from_secs(60);
/// A fetch of one branch, or a checkout of the whole tree.
const SLOW: Duration = Duration::from_secs(300);

/// Prefix of the throwaway worktree's directory name; the pid follows.
pub(super) const WORKTREE_PREFIX: &str = ".resync-";

/// The payload's surfaces in a consumer tree, as `git archive` pathspecs.
const SURFACES: &[&str] = &[".loom", ".claude/commands/loom"];

/// What the remote did with the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Push {
    /// The commit is on the default branch.
    Accepted,
    /// The branch moved since the fetch. Nothing was written.
    NonFastForward,
    /// Branch protection or a ruleset refused it; the text names the rule.
    Protected(String),
}

/// Run `git` in `dir` to an exit. `root` is the registered workspace, which
/// decides the forge credential the child gets (the same one a sweep child in
/// that repo gets). `Err` only for a spawn failure or a timeout.
fn run(dir: &Path, root: &Path, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    match run_bounded(cmd, timeout) {
        Ok(Completion::Exited(out)) => Ok(out),
        Ok(Completion::TimedOut { .. }) => {
            bail!("git {} timed out after {}s", args.join(" "), timeout.as_secs())
        }
        Err(e) => Err(anyhow!("git {}: {e}", args.join(" "))),
    }
}

/// [`run`], requiring exit 0. Returns trimmed stdout.
fn ok(dir: &Path, root: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let out = run(dir, root, args, timeout)?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The default branch's name (`main`), from `origin/HEAD`, else `main` when
/// `origin/main` exists. `None` when the clone has neither.
pub(super) fn default_branch(root: &Path) -> Option<String> {
    let head = run(
        root,
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        QUICK,
    )
    .ok()?;
    if head.status.success() {
        let name = String::from_utf8_lossy(&head.stdout).trim().to_string();
        if let Some(branch) = name.strip_prefix("origin/").filter(|b| !b.is_empty()) {
            return Some(branch.to_string());
        }
    }
    let main = run(
        root,
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/main",
        ],
        QUICK,
    )
    .ok()?;
    main.status.success().then(|| "main".to_string())
}

/// Fetch the default branch and return the commit `origin/<branch>` is at.
pub(super) fn fetch(root: &Path, branch: &str) -> Result<String> {
    // An explicit refspec, so the remote-tracking ref moves even in a clone
    // whose configured fetch refspec does not cover this branch.
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    ok(root, root, &["fetch", "--quiet", "--no-tags", "origin", &refspec], SLOW)
        .with_context(|| format!("fetching origin/{branch}"))?;
    // The checkout half of this pass (#10869) reuses this fetch.
    crate::fleet_sync::checkout_ff::note_fetched(root, branch);
    ok(
        root,
        root,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/origin/{branch}^{{commit}}"),
        ],
        QUICK,
    )
}

/// The tree of `commit`.
pub(super) fn tree_of(root: &Path, commit: &str) -> Result<String> {
    ok(root, root, &["rev-parse", "--verify", &format!("{commit}^{{tree}}")], QUICK)
}

/// The raw install metadata at `commit`; `None` when the tree has none.
pub(super) fn metadata_at(root: &Path, commit: &str) -> Result<Option<String>> {
    let spec = format!("{commit}:{INSTALL_METADATA_PATH}");
    if !run(root, root, &["cat-file", "-e", &spec], QUICK)?
        .status
        .success()
    {
        return Ok(None);
    }
    let out = run(root, root, &["show", &spec], QUICK)?;
    if !out.status.success() {
        bail!("git show {spec} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Write the payload surfaces of `commit` into `dest` (a temp dir), so the
/// payload can be diffed against the default branch without a checkout.
pub(super) fn export_surfaces(root: &Path, commit: &str, dest: &Path) -> Result<()> {
    let mut args = vec!["archive", "--format=tar", commit, "--"];
    for surface in SURFACES {
        let spec = format!("{commit}:{surface}");
        if run(root, root, &["cat-file", "-e", &spec], QUICK)?
            .status
            .success()
        {
            args.push(surface);
        }
    }
    let out = run(root, root, &args, SLOW)?;
    if !out.status.success() {
        bail!("git archive {commit} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    tar::Archive::new(out.stdout.as_slice())
        .unpack(dest)
        .with_context(|| format!("unpacking the default-branch tree into {}", dest.display()))
}

/// The subset of `paths` the workspace's ignore rules exclude. A file the
/// repo ignores can never be committed, so it is not a difference a resync
/// could close. Best effort: on any failure nothing is reported ignored.
pub(super) fn ignored(dir: &Path, root: &Path, paths: &[String]) -> BTreeSet<String> {
    if paths.is_empty() {
        return BTreeSet::new();
    }
    // Without `--no-index`: a tracked file is never "ignored", whatever the
    // patterns say, and a tracked payload file must keep being updated.
    let mut args = vec!["check-ignore", "--"];
    args.extend(paths.iter().map(String::as_str));
    // Exit 1 means "none ignored"; both 0 and 1 carry a usable stdout.
    run(dir, root, &args, QUICK).map_or_else(
        |_| BTreeSet::new(),
        |out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        },
    )
}

fn worktrees_dir(root: &Path) -> PathBuf {
    root.join(".loom").join("worktrees")
}

/// Remove every `.resync-*` worktree under `root`: a process killed mid-resync
/// leaves one behind, and one daemon per host means none is in use now.
pub(super) fn clean_stale_worktrees(root: &Path) {
    let Ok(entries) = std::fs::read_dir(worktrees_dir(root)) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(WORKTREE_PREFIX)
        {
            remove_worktree(root, &entry.path());
        }
    }
}

/// Add the throwaway worktree, detached at `commit`.
pub(super) fn add_worktree(root: &Path, commit: &str) -> Result<PathBuf> {
    let path = worktrees_dir(root).join(format!("{WORKTREE_PREFIX}{}", std::process::id()));
    let shown = path.to_string_lossy().into_owned();
    ok(root, root, &["worktree", "add", "--detach", &shown, commit], SLOW)
        .context("creating the resync worktree")?;
    Ok(path)
}

/// Remove a throwaway worktree and its registration. Never fails: whatever is
/// left is picked up by [`clean_stale_worktrees`] next time.
pub(super) fn remove_worktree(root: &Path, path: &Path) {
    let shown = path.to_string_lossy().into_owned();
    let removed = run(root, root, &["worktree", "remove", "--force", &shown], QUICK)
        .is_ok_and(|o| o.status.success());
    if !removed {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = run(root, root, &["worktree", "prune"], QUICK);
}

/// Stage exactly `written` in the worktree and commit it. Returns the new
/// commit, or `None` when no payload file is staged: either the repo ignores
/// everything the payload would add, or only the metadata stamp changed, and
/// a resync that changes no installed file writes nothing.
pub(super) fn commit(
    worktree: &Path,
    root: &Path,
    written: &[String],
    message: &str,
) -> Result<Option<String>> {
    let skip = ignored(worktree, root, written);
    let mut args = vec!["add", "-A", "--"];
    args.extend(
        written
            .iter()
            .filter(|p| !skip.contains(*p))
            .map(String::as_str),
    );
    if args.len() > 3 {
        ok(worktree, root, &args, QUICK).context("staging the resync")?;
    }
    let staged = ok(worktree, root, &["diff", "--cached", "--name-only"], QUICK)?;
    if !staged.lines().any(|p| p != INSTALL_METADATA_PATH) {
        return Ok(None);
    }
    // The repo's own identity when it has one (what every sweep commit on
    // this host uses); the daemon's name only when git has none configured.
    let has_identity = run(worktree, root, &["config", "user.email"], QUICK)
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
    let mut args: Vec<&str> = Vec::new();
    if !has_identity {
        args.extend([
            "-c",
            "user.name=loom-daemon",
            "-c",
            "user.email=loom-daemon@users.noreply.github.com",
        ]);
    }
    args.extend(["commit", "--quiet", "-m", message]);
    ok(worktree, root, &args, SLOW).context("committing the resync")?;
    ok(worktree, root, &["rev-parse", "HEAD"], QUICK).map(Some)
}

/// Push the worktree's HEAD to the default branch. Never forced, hooks run.
///
/// # Errors
/// git could not run, or the push failed for a reason that is neither a moved
/// branch nor a protection rule (network, auth).
pub(super) fn push(worktree: &Path, root: &Path, branch: &str) -> Result<Push> {
    let refspec = format!("HEAD:refs/heads/{branch}");
    let out = run(worktree, root, &["push", "origin", &refspec], SLOW)?;
    if out.status.success() {
        return Ok(Push::Accepted);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    match classify_rejection(&stderr) {
        Some(push) => Ok(push),
        None => bail!("git push origin {refspec} failed: {}", stderr.trim()),
    }
}

/// Read a failed push's stderr. `None` when it is neither kind of rejection.
pub(super) fn classify_rejection(stderr: &str) -> Option<Push> {
    let lower = stderr.to_lowercase();
    let names_a_rule = |line: &&str| {
        let l = line.to_lowercase();
        ["gh006", "gh013", "protected branch", "rule violation"]
            .iter()
            .any(|needle| l.contains(needle))
    };
    if let Some(line) = stderr.lines().find(names_a_rule) {
        let rule = line.trim().trim_start_matches("remote:").trim();
        return Some(Push::Protected(rule.to_string()));
    }
    if let Some(line) = stderr.lines().find(|l| l.contains("[remote rejected]")) {
        // Two pushes racing for the ref: the loser's update cannot take the
        // lock. That is a moved branch, not a rule.
        if line.to_lowercase().contains("lock") {
            return Some(Push::NonFastForward);
        }
        return Some(Push::Protected(line.trim().to_string()));
    }
    (lower.contains("non-fast-forward") || lower.contains("fetch first"))
        .then_some(Push::NonFastForward)
}
