//! The read-only half of the self-heal (#11425): decide whether everything
//! that blocks a fast-forward is Loom's own, and say what it is.
//!
//! Nothing here writes. Every doubt is a [`Verdict::Leave`]: unreadable
//! metadata, a path git cannot name, an operation the module does not know,
//! more commits than it will read. A checkout that is left is reported by the
//! ordinary rules, exactly as before.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use super::super::{git, Env};
use super::managed;

/// How old an operation or an `index.lock` must be before it is called stale.
pub(super) const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

/// More local commits than this are not read one by one: left alone.
const MAX_COMMITS: usize = 500;

const METADATA: &str = ".loom/install-metadata.json";

/// A git operation left in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    Rebase,
    Merge,
    CherryPick,
}

impl Op {
    pub(super) fn abort_args(self) -> [&'static str; 2] {
        match self {
            Self::Rebase => ["rebase", "--abort"],
            Self::Merge => ["merge", "--abort"],
            Self::CherryPick => ["cherry-pick", "--abort"],
        }
    }
}

/// What a repair would do. Every blocker in it is Loom-owned.
#[derive(Debug)]
pub(super) struct Plan {
    /// A stale, empty `index.lock` to remove.
    pub lock: Option<PathBuf>,
    /// Its age in minutes, for the log.
    pub lock_age_min: u64,
    pub op: Option<Op>,
    /// Local commits that `reset --hard` will leave only in the rescue ref.
    pub commits: usize,
    /// Tracked paths with a staged or unstaged change.
    pub dirty: Vec<String>,
    /// Does the repair need to reset (commits, dirt or an operation), or only
    /// remove the lock?
    pub reset: bool,
    /// Does the branch ref differ from HEAD (an operation moved HEAD)?
    pub branch_differs: bool,
    /// HEAD is detached with no operation in progress, and has sat there for
    /// [`managed::detached_after`] (#11989): the repair puts it back on the
    /// branch.
    pub detached: bool,
}

#[derive(Debug)]
pub(super) enum Verdict {
    /// Nothing blocks a fast-forward that this module is about.
    Clean,
    /// All of it is Loom's: repair.
    Repair(Plan),
    /// Something is not Loom's, or cannot be shown to be. Why, for the log.
    Leave(String),
}

fn local(env: &Env<'_>) -> Duration {
    env.bound(git::LOCAL_TIMEOUT)
}

fn read(env: &Env<'_>, root: &Path, args: &[&str]) -> Result<String, String> {
    git::ok(root, args, local(env))
}

/// The absolute git directory.
pub(super) fn git_dir(env: &Env<'_>, root: &Path) -> Option<PathBuf> {
    read(env, root, &["rev-parse", "--absolute-git-dir"])
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// Which operation is in progress. `Err(())`: one is, but not one this module
/// repairs (bisect, revert, `am`, a multi-commit sequencer, several at once).
fn operation(gd: &Path) -> Result<Option<Op>, ()> {
    let has = |m: &str| gd.join(m).exists();
    let known: Vec<Op> = [
        (has("rebase-merge"), Op::Rebase),
        (has("MERGE_HEAD"), Op::Merge),
        (has("CHERRY_PICK_HEAD"), Op::CherryPick),
    ]
    .into_iter()
    .filter_map(|(present, op)| present.then_some(op))
    .collect();
    let other = ["rebase-apply", "BISECT_LOG", "REVERT_HEAD", "sequencer"]
        .iter()
        .any(|m| has(m));
    match (known.as_slice(), other) {
        ([], false) => Ok(None),
        ([op], false) => Ok(Some(*op)),
        _ => Err(()),
    }
}

/// The newest modification time among the operation's own state files.
fn op_touched(gd: &Path, op: Op) -> Option<SystemTime> {
    let marker = match op {
        Op::Rebase => "rebase-merge",
        Op::Merge => "MERGE_HEAD",
        Op::CherryPick => "CHERRY_PICK_HEAD",
    };
    let path = gd.join(marker);
    let mut newest = path.metadata().ok()?.modified().ok()?;
    if path.is_dir() {
        for entry in std::fs::read_dir(&path).ok()?.flatten() {
            newest = newest.max(entry.metadata().ok()?.modified().ok()?);
        }
    }
    Some(newest)
}

/// Is `touched` at least [`STALE_AFTER`] before `now`? A time in the future,
/// or unknown, is young.
fn stale(touched: Option<SystemTime>, now: DateTime<Utc>) -> bool {
    let Some(touched) = touched else {
        return false;
    };
    let then = DateTime::<Utc>::from(touched);
    now.signed_duration_since(then)
        .to_std()
        .is_ok_and(|age| age >= STALE_AFTER)
}

/// The Loom-managed paths: `installed_files` of the install metadata at every
/// revision in `revs`, plus the metadata file. `Err` when any is missing or
/// unparseable. A path outside this set may still be Loom's: see
/// [`managed::resync_owned`].
fn owned_paths(env: &Env<'_>, root: &Path, revs: &[String]) -> Result<BTreeSet<String>, String> {
    let mut owned = BTreeSet::from([METADATA.to_string()]);
    for rev in revs {
        let raw = read(env, root, &["show", &format!("{rev}:{METADATA}")])
            .map_err(|e| format!("no install metadata at {rev}: {e}"))?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("install metadata at {rev}: {e}"))?;
        let files = value
            .get("installed_files")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("install metadata at {rev} has no installed_files"))?;
        owned.extend(files.iter().filter_map(|f| f.as_str().map(str::to_string)));
    }
    Ok(owned)
}

/// Every path a commit changes (both sides of a rename, every parent of a
/// merge).
fn commit_paths(env: &Env<'_>, root: &Path, sha: &str) -> Result<Vec<String>, String> {
    let raw = read(
        env,
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-m",
            "--root",
            "--no-renames",
            "-z",
            sha,
        ],
    )?;
    Ok(raw
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

/// Tracked paths with a staged or unstaged change, one entry per path.
fn dirty_paths(env: &Env<'_>, root: &Path) -> Result<Vec<String>, String> {
    let args = [
        "--no-optional-locks",
        "status",
        "--porcelain",
        "-z",
        "--untracked-files=no",
        "--no-renames",
    ];
    // Not `read`: its trim would eat the leading space of ` M path`.
    let out = git::run(root, &[], &args, local(env))?;
    if !out.status.success() {
        return Err(format!("git status failed: {}", git::stderr(&out)));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(text
        .split('\0')
        .filter(|r| r.len() > 3)
        .map(|r| r[3..].to_string())
        .collect())
}

/// The commits an in-progress rebase has done or will do.
fn rebase_commits(gd: &Path) -> Result<Vec<String>, String> {
    let mut shas = Vec::new();
    for file in ["rebase-merge/done", "rebase-merge/git-rebase-todo"] {
        let Ok(text) = std::fs::read_to_string(gd.join(file)) else {
            continue;
        };
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("noop"), _) => {}
                (
                    Some(
                        "pick" | "p" | "reword" | "r" | "edit" | "e" | "squash" | "s" | "fixup"
                        | "f" | "drop" | "d",
                    ),
                    Some(sha),
                ) => shas.push(sha.to_string()),
                // exec, label, reset, merge, break, update-ref: not Loom's to judge.
                _ => return Err(format!("the rebase has a `{line}` step")),
            }
        }
    }
    Ok(shas)
}

/// Is a `git` process running with its working directory inside `root`?
/// Fails closed: a process whose directory cannot be read counts as live.
pub(super) fn live_git(root: &Path) -> bool {
    let Ok(out) = Command::new("pgrep").args(["-x", "git"]).output() else {
        return true;
    };
    match out.status.code() {
        Some(1) => return false,
        Some(0) => {}
        _ => return true,
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse::<i32>().ok())
        .any(|pid| match cwd_of(pid) {
            Some(cwd) => cwd.starts_with(&root),
            None => process_exists(pid),
        })
}

fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn cwd_of(pid: i32) -> Option<PathBuf> {
    if let Ok(link) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
        return Some(link);
    }
    let out = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p)))
}

/// Would `reset --hard origin/<branch>` overwrite an untracked file? Those are
/// never touched, so one in the way means no repair.
fn untracked_in_the_way(
    env: &Env<'_>,
    root: &Path,
    branch: &str,
) -> Result<Option<String>, String> {
    let tracking = git::tracking(branch);
    let changed = read(
        env,
        root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            "HEAD",
            &tracking,
        ],
    )?;
    let tracked = read(env, root, &["ls-files", "-z"])?;
    let tracked: BTreeSet<&str> = tracked.split('\0').collect();
    Ok(changed
        .split('\0')
        .filter(|p| !p.is_empty() && !tracked.contains(p))
        .find(|p| root.join(p).symlink_metadata().is_ok())
        .map(str::to_string))
}

/// Classify what blocks `root`'s fast-forward to `origin/<branch>`.
pub(super) fn classify(env: &Env<'_>, root: &Path, branch: &str, gd: &Path) -> Verdict {
    match classify_inner(env, root, branch, gd) {
        Ok(v) => v,
        Err(why) => Verdict::Leave(why),
    }
}

fn classify_inner(env: &Env<'_>, root: &Path, branch: &str, gd: &Path) -> Result<Verdict, String> {
    let now = (env.clock)();
    let op =
        operation(gd).map_err(|()| "an operation this repair does not handle is in progress")?;
    let lock = gd.join("index.lock");
    let lock = lock.exists().then_some(lock);
    let tracking = git::tracking(branch);
    let own_ref = format!("refs/heads/{branch}");
    let mut commits: BTreeSet<String> = BTreeSet::new();
    for tip in [own_ref.as_str(), "HEAD"] {
        let listed = read(env, root, &["rev-list", &format!("{tracking}..{tip}")]);
        commits.extend(listed.unwrap_or_default().lines().map(str::to_string));
    }
    let dirty = dirty_paths(env, root)?;
    // #11989: a HEAD left detached with nothing in progress (a role session's
    // `git checkout origin/main` or of a PR head in the main checkout). During
    // a rebase HEAD is detached by design; a merge or cherry-pick on a
    // detached HEAD is not one this repair knows how to put back.
    let current = git::current_branch(root, local(env))?;
    let detached = current.is_none();
    if detached && matches!(op, Some(Op::Merge | Op::CherryPick)) {
        return Err("a merge or cherry-pick is in progress on a detached HEAD".to_string());
    }
    let stray = detached && op.is_none();
    // Dirt only blocks a fast-forward when there is something to fast-forward
    // to; on a checkout that is not behind it is the operator's to keep. (The
    // clone's own ref is what is compared: a fetch this pass has not made yet
    // is seen by the next one.)
    let behind = read(env, root, &["rev-list", "--count", &format!("HEAD..{tracking}")])
        .ok()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    let blocking_dirt = !dirty.is_empty() && behind > 0;
    // A detached HEAD only blocks when it is behind: one at origin's tip (a
    // tool pinning the checkout to a current commit) serves current files.
    let stray_behind = stray && behind > 0;
    if lock.is_none() && op.is_none() && commits.is_empty() && !blocking_dirt && !stray_behind {
        return Ok(Verdict::Clean);
    }
    // The repair resets whatever HEAD is on. A checkout someone has on another
    // named branch is theirs, however Loom-like its commits look.
    if let Some(on) = current.as_deref().filter(|on| *on != branch) {
        return Err(format!("the checkout is on `{on}`, not `{branch}`"));
    }
    if commits.len() > MAX_COMMITS {
        return Err(format!("{} local commits are more than are read", commits.len()));
    }

    // Whose is it? Fail closed on the metadata.
    let revs = ["HEAD".to_string(), own_ref.clone(), tracking.clone()];
    let listed = owned_paths(env, root, &revs)?;
    let pins = managed::pins(root);
    // `rev` is the commit the path was changed in; `None` is the working tree.
    // A skill the payload ships is the resync's only while it carries the
    // generation marker (`resync_agent_skills`); a consumer's own, or one
    // detached from generation by stripping it, is not.
    let owned = |rev: Option<&str>, p: &str| {
        if listed.contains(p) {
            return true;
        }
        if !managed::resync_owned(p, &pins) {
            return false;
        }
        if !managed::is_skill(p) {
            return true;
        }
        let content = match rev {
            None => std::fs::read_to_string(root.join(p)).ok(),
            Some(sha) => read(env, root, &["show", &format!("{sha}:{p}")])
                .or_else(|_| read(env, root, &["show", &format!("{sha}^:{p}")]))
                .ok(),
        };
        content.is_some_and(|c| managed::has_marker(&c))
    };
    let mut judged: Vec<String> = commits.iter().cloned().collect();
    if op == Some(Op::Rebase) {
        let head_name =
            std::fs::read_to_string(gd.join("rebase-merge/head-name")).unwrap_or_default();
        if head_name.trim() != own_ref {
            return Err(format!("the rebase is of `{}`, not `{branch}`", head_name.trim()));
        }
        judged.extend(rebase_commits(gd)?);
    }
    match op {
        Some(Op::Merge) => judged.push(read(env, root, &["rev-parse", "MERGE_HEAD"])?),
        Some(Op::CherryPick) => judged.push(read(env, root, &["rev-parse", "CHERRY_PICK_HEAD"])?),
        _ => {}
    }
    if op == Some(Op::Merge) {
        // The commits the merge brings in, not only the one it names.
        let brought = read(env, root, &["rev-list", "HEAD..MERGE_HEAD", "--not", &tracking])?;
        judged.extend(brought.lines().map(str::to_string));
    }
    for sha in &judged {
        if let Some(path) = commit_paths(env, root, sha)?
            .into_iter()
            .find(|p| !owned(Some(sha), p))
        {
            return Err(format!("commit {sha} changes `{path}`, which Loom does not manage"));
        }
    }
    if let Some(path) = dirty.iter().find(|p| !owned(None, p)) {
        return Err(format!("`{path}` has an uncommitted change and Loom does not manage it"));
    }

    // Is it stale, and is nothing alive in there?
    if let Some(op) = op {
        if !stale(op_touched(gd, op), now) {
            return Err("the operation in progress is recent".to_string());
        }
    }
    if let Some(lock) = &lock {
        let empty = lock.metadata().is_ok_and(|m| m.len() == 0);
        if !empty || !stale(lock.metadata().and_then(|m| m.modified()).ok(), now) {
            return Err("index.lock is young or not empty".to_string());
        }
    }
    // Re-attached only when the repair resets anyway, or the HEAD is behind.
    let reattach = stray && (stray_behind || !commits.is_empty() || blocking_dirt);
    if reattach {
        // A tool that pins a checkout to a commit on purpose (2am's
        // `provision.sh` keeps geode-fem at the newest green one) says so here;
        // while main is red such a HEAD is detached, behind and unmoved, which
        // is exactly the shape this repair puts back.
        let keep = read(env, root, &["config", "--local", "--get", "loom.checkoutFf.detached"])
            .unwrap_or_default();
        if keep.trim().eq_ignore_ascii_case("keep") {
            return Err("HEAD is detached on purpose (loom.checkoutFf.detached=keep)".to_string());
        }
        let moved = managed::head_moved_at(gd);
        if !moved.is_some_and(|at| {
            now.signed_duration_since(at)
                .to_std()
                .is_ok_and(|age| age >= managed::detached_after())
        }) {
            return Err("HEAD is detached and moved recently".to_string());
        }
    }
    if (op.is_some() || lock.is_some() || reattach) && live_git(root) {
        return Err("a git process is running in the repository".to_string());
    }
    if let Some(path) = untracked_in_the_way(env, root, branch)? {
        return Err(format!("an untracked file `{path}` is where origin writes one"));
    }

    let lock_age_min = lock
        .as_ref()
        .and_then(|l| l.metadata().ok()?.modified().ok())
        .map_or(0, |t| {
            now.signed_duration_since(DateTime::<Utc>::from(t))
                .num_minutes()
                .max(0) as u64
        });
    let head = read(env, root, &["rev-parse", "HEAD"])?;
    let tip = read(env, root, &["rev-parse", "--verify", "--quiet", &own_ref]).unwrap_or_default();
    Ok(Verdict::Repair(Plan {
        reset: op.is_some() || !commits.is_empty() || blocking_dirt || reattach,
        lock,
        lock_age_min,
        op,
        commits: commits.len(),
        dirty,
        branch_differs: !tip.is_empty() && tip != head,
        detached: reattach,
    }))
}
