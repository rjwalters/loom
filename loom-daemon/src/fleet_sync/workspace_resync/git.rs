//! The git half of the workspace resync (#10718): everything it runs in a
//! registered workspace's clone. Reads go through the clone's object store
//! and remote-tracking refs; the only writes are a `fetch` of the default
//! branch (when its head moved), a throwaway detached worktree under
//! `.loom/worktrees/`, and a push that is never forced. The operator's
//! checkout (its index, working tree and `FETCH_HEAD`) is not touched by
//! anything here.
//!
//! Every child runs under a short timeout (see the constants below), and a
//! timed-out child's process group is killed. Every ref operand derived from
//! the remote sits behind a standalone `--` (#9106, #9479), and the default
//! branch's name is checked with [`crate::refname::check_refname`] first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::install_compat::INSTALL_METADATA_PATH;
use crate::proc_exec::{run_bounded, Completion};

/// Local plumbing: no network, small output.
const QUICK: Duration = Duration::from_secs(20);
/// `git ls-remote` of one ref: a connection and a ref advertisement.
const PROBE: Duration = Duration::from_secs(15);
/// A fetch of one branch.
const FETCH: Duration = Duration::from_secs(30);
/// A checkout of the whole tree, an archive of it, a commit or a push (the
/// last two run the repo's hooks).
const HEAVY: Duration = Duration::from_secs(60);

/// How many commits of the default branch [`past_resyncs`] reads.
const HISTORY_DEPTH: &str = "--max-count=300";

/// Trailer naming the host that made a resync commit.
pub(super) const TRAILER_HOST: &str = "Loom-Resync-Host";
/// Trailer naming the version a resync commit installed.
pub(super) const TRAILER_VERSION: &str = "Loom-Resync-Version";

/// The remote could not be reached (or did not answer in time). Kept apart
/// from every other failure so an outage is one host-level alert, not one
/// per repo.
#[derive(Debug)]
pub(super) struct Unreachable(pub(super) String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

/// The remote (or the forge) answered and refused this repo: it was deleted
/// or renamed, or the credential may not read it. Kept apart from
/// [`Unreachable`] so one dead repo is that repo's failure and never counts
/// toward the host's network outage (#10987).
#[derive(Debug)]
pub(super) struct Refused(pub(super) String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Does a failed `ls-remote` or `fetch` say the remote answered and refused,
/// as opposed to not answering? Read from git's own stderr: GitHub's "not
/// found" (which is also its answer to a credential that may not see the
/// repo) and the HTTP and credential-helper authentication failures.
pub(super) fn is_refusal(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    [
        "repository not found",
        "authentication failed",
        "could not read username",
        "could not read password",
        "invalid username or",
        "bad credentials",
        "permission to ",
        "returned error: 401",
        "returned error: 403",
        "returned error: 404",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// The error for a network child (`ls-remote`, `fetch`) that exited non-zero.
fn network_failure(what: &str, stderr: &[u8]) -> anyhow::Error {
    let detail = format!("{what} failed: {}", first_line(stderr));
    if is_refusal(&String::from_utf8_lossy(stderr)) {
        Refused(detail).into()
    } else {
        Unreachable(detail).into()
    }
}

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
        // The name is the remote's to choose. One git would read as an
        // option or a revision expression is not used (#9106).
        if let Some(branch) = name
            .strip_prefix("origin/")
            .filter(|b| crate::refname::check_refname(b).is_ok())
        {
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

/// The first line of a child's stderr: enough to say why the remote did not
/// answer, without git's advice paragraphs.
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string()
}

/// The commit `refs/heads/<branch>` is at on the remote, by `git ls-remote`:
/// one connection, no objects, nothing written in the clone.
///
/// # Errors
/// [`Unreachable`] when the remote did not answer; [`Refused`] when it
/// answered and refused the repo; a plain error when it answered without
/// that branch.
pub(super) fn remote_head(root: &Path, branch: &str) -> Result<String> {
    let name = format!("refs/heads/{branch}");
    let out = run(root, root, &["ls-remote", "--quiet", "origin", "--", &name], PROBE)
        .map_err(|e| Unreachable(format!("{e:#}")))?;
    if !out.status.success() {
        return Err(network_failure("git ls-remote origin", &out.stderr));
    }
    let head = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(sha, listed)| *listed == name && sha.len() >= 40)
        .map(|(sha, _)| sha.to_string())
        .ok_or_else(|| anyhow!("origin has no {name}"))?;
    // The checkout half (#10869) does not ask again for what was just answered.
    crate::fleet_sync::checkout_ff::note_remote_head(root, branch, &head);
    Ok(head)
}

/// The commit the clone's `origin/<branch>` is at; `None` when it has never
/// been fetched. Local.
pub(super) fn tracking_head(root: &Path, branch: &str) -> Option<String> {
    let spec = format!("refs/remotes/origin/{branch}^{{commit}}");
    let out = run(root, root, &["rev-parse", "--verify", "--quiet", &spec], QUICK).ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

/// Fetch the default branch and return the commit `origin/<branch>` is at.
///
/// # Errors
/// [`Unreachable`] when the fetch got no answer; [`Refused`] when the remote
/// refused the repo.
pub(super) fn fetch(root: &Path, branch: &str) -> Result<String> {
    // An explicit refspec, so the remote-tracking ref moves even in a clone
    // whose configured fetch refspec does not cover this branch. Behind `--`
    // because the branch name is the remote's. `--no-write-fetch-head`: this
    // is the operator's checkout, and its `FETCH_HEAD` is theirs.
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let args = [
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-write-fetch-head",
        "origin",
        "--",
        &refspec,
    ];
    let out = run(root, root, &args, FETCH).map_err(|e| Unreachable(format!("{e:#}")))?;
    if !out.status.success() {
        return Err(network_failure(&format!("fetching origin/{branch}"), &out.stderr));
    }
    tracking_head(root, branch)
        .ok_or_else(|| anyhow!("origin/{branch} is missing after a successful fetch"))
}

/// A daemon resync commit found on the default branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PastResync {
    /// The commit.
    pub(super) commit: String,
    /// Its `Loom-Resync-Host`.
    pub(super) host: String,
    /// Its `Loom-Resync-Version`.
    pub(super) version: String,
}

/// The message of a resync commit.
pub(super) fn resync_message(host: &str, version: &str) -> String {
    format!(
        "chore(loom): resync installed Loom to v{version}\n\n{TRAILER_HOST}: {host}\n\
         {TRAILER_VERSION}: {version}\n"
    )
}

/// The daemon resync commits among the last [`HISTORY_DEPTH`] commits reachable
/// from `commit`, newest first. Local: the history is already in the clone.
pub(super) fn past_resyncs(root: &Path, commit: &str) -> Result<Vec<PastResync>> {
    let args = ["log", HISTORY_DEPTH, "--format=%x1e%H%x1f%B", commit, "--"];
    let out = ok(root, root, &args, QUICK).context("reading the default branch's history")?;
    Ok(out.split('\u{1e}').filter_map(parse_past_resync).collect())
}

fn parse_past_resync(record: &str) -> Option<PastResync> {
    let (commit, body) = record.split_once('\u{1f}')?;
    let trailer = |key: &str| {
        body.lines()
            .filter_map(|line| line.split_once(": "))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(PastResync {
        commit: commit.trim().to_string(),
        version: trailer(TRAILER_VERSION)?,
        host: trailer(TRAILER_HOST).unwrap_or_else(|| "an unnamed host".to_string()),
    })
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
    let out = run(root, root, &args, HEAVY)?;
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

/// The throwaway worktree. Removed when dropped, so a panic or an early
/// return inside the resync leaves nothing behind.
pub(super) struct Worktree {
    root: PathBuf,
    path: PathBuf,
}

impl Worktree {
    /// Where it is.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        remove_worktree(&self.root, &self.path);
    }
}

/// Add the throwaway worktree, detached at `commit`.
pub(super) fn add_worktree(root: &Path, commit: &str) -> Result<Worktree> {
    let path = worktrees_dir(root).join(format!("{WORKTREE_PREFIX}{}", std::process::id()));
    let shown = path.to_string_lossy().into_owned();
    // The guard exists before the checkout starts: one that fails or times
    // out part-way is removed too.
    let worktree = Worktree {
        root: root.to_path_buf(),
        path,
    };
    ok(root, root, &["worktree", "add", "--detach", &shown, commit], HEAVY)
        .context("creating the resync worktree")?;
    Ok(worktree)
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
    ok(worktree, root, &args, HEAVY).context("committing the resync")?;
    ok(worktree, root, &["rev-parse", "HEAD"], QUICK).map(Some)
}

/// Push the worktree's HEAD to the default branch. Never forced, hooks run.
///
/// # Errors
/// git could not run, or the push failed for a reason that is neither a moved
/// branch nor a protection rule (network, auth).
pub(super) fn push(worktree: &Path, root: &Path, branch: &str) -> Result<Push> {
    let refspec = format!("HEAD:refs/heads/{branch}");
    let out = run(worktree, root, &["push", "origin", "--", &refspec], HEAVY)?;
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
