//! Remote backup of an idle **open** item's worktree, taken before the reaper
//! removes it (issue #12458; operator direction 2026-10-20: "first synchronize
//! to a remote github branch so that we would never lose data").
//!
//! The idle-open decision ([`super::clean::classify_worktree`] /
//! [`super::clean::classify_pr_worktree`]) only reaches
//! [`WorktreeDecision::RemoveWithBackup`](super::clean::WorktreeDecision::RemoveWithBackup)
//! for a worktree its probes found clean, idle and fully pushed — but every one
//! of those probes is an *inference* (`git diff`, `git ls-files --others`, a
//! `rev-list --not --remotes` count, an mtime walk), and `git worktree remove
//! --force` is one-way. This module is the proof the decision rests on
//! instead: before anything is deleted, the worktree's **entire** state is
//! committed and pushed to the remote, and the remote is read back to confirm
//! it holds that exact commit. A failure at any step is `Err`, and the caller
//! keeps the worktree — fail closed, never "probably fine".
//!
//! # What is captured
//!
//! A snapshot commit built in a **temporary index** — the worktree's own
//! index, `HEAD`, branch and `refs/stash` are never touched:
//!
//! 1. `read-tree HEAD` into the temporary index;
//! 2. `add --all` over the whole tree, so tracked modifications, deletions,
//!    staged-only changes, intent-to-add entries and every **untracked,
//!    non-ignored** file land in it. `.gitignore` / `info/exclude` are honoured
//!    exactly as a user's `git add -A` would honour them, so an ignored file is
//!    never pushed. Also excluded: a content-verified Cargo build tree
//!    ([`crate::generated_artifact::worktree_build_tree_dirs`], the #11075
//!    rule the quarantine stash already applies) and Loom's own runtime
//!    markers ([`super::safety::LOOM_OWN_UNTRACKED_FILES`]);
//! 3. `write-tree`; when the tree equals `HEAD`'s, the snapshot **is** `HEAD`
//!    (nothing to commit, the backup ref still anchors the branch tip);
//!    otherwise `commit-tree <tree> -p HEAD`, a detached commit whose parent
//!    is the worktree's `HEAD`. The branch's own history is unchanged.
//!
//! Because the snapshot's parent is `HEAD`, pushing it also pushes every
//! commit on the branch the remote did not already have — the "unpushed
//! commits" case is covered by the same ref, without ever writing to the
//! branch's own `refs/heads/*` (which would re-trigger CI and move a PR head).
//!
//! # Where it goes
//!
//! `refs/loom/backup/<issue-N | pr-N>/<branch | detached>/<UTC stamp>` on
//! `origin`, the remote every Loom path (`worktree.sh`, `merge-pr.sh`) already
//! assumes. The namespace, not a `loom-backup/...` branch, because
//!
//! - GitHub Actions `push` events, branch protection, required checks, PR
//!   automation and the merge queue all key on `refs/heads/*` / `refs/tags/*`;
//!   a custom namespace triggers none of them;
//! - a clone's default fetch refspec (`refs/heads/*`) never pulls them down, so
//!   backups cost nobody disk;
//! - `refs/loom/` is already the daemon's own namespace
//!   (`refs/loom/stash-baseline/*`, `worktree_cli::baseline`);
//! - GitHub accepts pushes to it (verified against `rjwalters/loom` on
//!   2026-10-20: a push to `refs/loom/backup/selftest-12466/<stamp>` was
//!   created, listed by `ls-remote`, and deleted).
//!
//! Restore: `git fetch origin <ref>` then `git checkout FETCH_HEAD` (or
//! `git stash apply`-style `git checkout FETCH_HEAD -- .` onto the recreated
//! branch). The ref name, the sha, the parent `HEAD` and the branch are logged
//! by the reaper and written to the removal ledger (#5950), so the restore
//! never depends on anyone remembering the stamp.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

/// The remote namespace every backup ref lives under.
pub const BACKUP_REF_PREFIX: &str = "refs/loom/backup";

/// The remote the backup is pushed to. `origin` is what `worktree.sh` resumes
/// `feature/issue-N` from and what every fleet credential helper is set up
/// for; a repo without one cannot be backed up and is therefore never reaped
/// through this path.
pub const BACKUP_REMOTE: &str = "origin";

/// Identity on the snapshot commit — the reaper's, not a user's, and set
/// explicitly so a host with no `user.name` can still build it.
const SNAPSHOT_AUTHOR: &str = "loom worktree reaper";
const SNAPSHOT_EMAIL: &str = "loom-daemon@localhost";

/// The reaper tick must never hang on a dead network or an auth prompt.
const PUSH_TIMEOUT: Duration = Duration::from_secs(180);
const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(100);

/// What one backup pushed — everything a person needs to restore it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRecord {
    /// The remote the ref was pushed to ([`BACKUP_REMOTE`]).
    pub remote: String,
    /// The full backup ref on that remote.
    pub ref_name: String,
    /// The commit the ref points at: the snapshot, or `head` itself when the
    /// worktree held nothing beyond `HEAD`.
    pub sha: String,
    /// The worktree's `HEAD` at backup time (the snapshot's parent).
    pub head: String,
    /// The checked-out branch, `None` for a detached `HEAD`.
    pub branch: Option<String>,
}

impl BackupRecord {
    /// Whether a snapshot commit was needed, i.e. the worktree held tracked
    /// or untracked changes beyond `HEAD`.
    #[must_use]
    pub fn captured_changes(&self) -> bool {
        self.sha != self.head
    }

    /// One line for the reaper log and the removal ledger: remote, ref, sha,
    /// parent, branch, and the restore command.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "backup {}:{} = {} (parent {}, branch {}; restore: git fetch {} {} && git checkout \
             FETCH_HEAD)",
            self.remote,
            self.ref_name,
            self.sha,
            self.head,
            self.branch.as_deref().unwrap_or("<detached>"),
            self.remote,
            self.ref_name,
        )
    }
}

/// The backup ref for `item` (`issue-N` / `pr-N`) on `branch` at `at`.
#[must_use]
pub fn backup_ref_name(item: &str, branch: Option<&str>, at: DateTime<Utc>) -> String {
    format!(
        "{BACKUP_REF_PREFIX}/{item}/{}/{}",
        branch.unwrap_or("detached"),
        at.format("%Y%m%dT%H%M%SZ")
    )
}

/// Snapshot `worktree`'s whole state, push it to [`BACKUP_REMOTE`] under
/// [`backup_ref_name`], and confirm the remote holds that exact sha.
///
/// `Err` means **nothing may be removed**: the push was rejected, the remote
/// is unreachable or unconfigured, `git` failed or timed out, or the remote
/// read back a different sha. The worktree itself is never modified — only
/// a temporary index file is written, and it is removed on every path.
pub fn backup_to_remote(
    worktree: &Path,
    item: &str,
    at: DateTime<Utc>,
) -> Result<BackupRecord, String> {
    if !worktree.is_dir() {
        return Err(format!("{} is not a directory", worktree.display()));
    }
    let head = git_stdout(worktree, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .map_err(|e| format!("cannot resolve HEAD: {e}"))?;
    let branch = git_stdout(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty());
    git_stdout(worktree, &["remote", "get-url", BACKUP_REMOTE])
        .map_err(|e| format!("remote '{BACKUP_REMOTE}' is not configured: {e}"))?;

    let ref_name = backup_ref_name(item, branch.as_deref(), at);
    git_stdout(worktree, &["check-ref-format", &ref_name])
        .map_err(|e| format!("backup ref name '{ref_name}' is not valid: {e}"))?;

    let sha = snapshot_commit(worktree, &head, item, branch.as_deref(), at)?;

    // `--no-verify`: a repo's pre-push hook is the author's gate on *their*
    // pushes, not on the reaper's backup of their work. `GIT_TERMINAL_PROMPT=0`
    // turns a missing credential into a fast failure instead of a hang.
    let refspec = format!("{sha}:{ref_name}");
    run_git_with_timeout(
        worktree,
        &["push", "--quiet", "--no-verify", BACKUP_REMOTE, &refspec],
        &[("GIT_TERMINAL_PROMPT", "0")],
        PUSH_TIMEOUT,
    )
    .map_err(|e| format!("push of {ref_name} to {BACKUP_REMOTE} failed: {e}"))?;

    let listed = run_git_with_timeout(
        worktree,
        &["ls-remote", "--exit-code", BACKUP_REMOTE, &ref_name],
        &[("GIT_TERMINAL_PROMPT", "0")],
        LS_REMOTE_TIMEOUT,
    )
    .map_err(|e| format!("could not verify {ref_name} on {BACKUP_REMOTE}: {e}"))?;
    let remote_sha = listed
        .lines()
        .find_map(|line| {
            let mut cols = line.split_whitespace();
            let sha = cols.next()?;
            (cols.next()? == ref_name).then(|| sha.to_string())
        })
        .ok_or_else(|| format!("{BACKUP_REMOTE} did not list {ref_name} after the push"))?;
    if remote_sha != sha {
        return Err(format!("{BACKUP_REMOTE} holds {remote_sha} at {ref_name}, expected {sha}"));
    }

    Ok(BackupRecord {
        remote: BACKUP_REMOTE.to_string(),
        ref_name,
        sha,
        head,
        branch,
    })
}

/// The snapshot commit for `worktree` — `head` itself when the tree holds
/// nothing beyond it. See the module docs for what is and is not captured.
fn snapshot_commit(
    worktree: &Path,
    head: &str,
    item: &str,
    branch: Option<&str>,
    at: DateTime<Utc>,
) -> Result<String, String> {
    let index = TempIndex::new()?;
    let env = [("GIT_INDEX_FILE", index.path_str())];
    git_env_stdout(worktree, &env, &["read-tree", head])
        .map_err(|e| format!("read-tree into the temporary index failed: {e}"))?;

    // Tracked side: every modification and deletion of a path in `HEAD`.
    git_env_stdout(worktree, &env, &["add", "--update", "--", ":/"])
        .map_err(|e| format!("staging tracked changes into the temporary index failed: {e}"))?;

    // Untracked side, listed against the temporary index (so an intent-to-add
    // entry, which the real index knows but `HEAD` does not, is "other" here
    // and gets its content). `--exclude-standard` is the user's own ignore
    // rules; the two Loom filters are applied by name, never as a pathspec —
    // `git add` exits 1 when a pathspec names only ignored files, which is
    // exactly what `.loom-managed` is in a repo with the `loom-managed`
    // `.gitignore` block.
    let others =
        git_env_stdout(worktree, &env, &["ls-files", "--others", "--exclude-standard", "-z"])
            .map_err(|e| format!("listing untracked files failed: {e}"))?;
    let build_trees = crate::generated_artifact::worktree_build_tree_dirs(worktree);
    let mut to_add: Vec<&str> = Vec::new();
    for path in others.split('\0').filter(|p| !p.is_empty()) {
        if super::safety::is_loom_own_untracked_path(path) {
            continue;
        }
        if build_trees.iter().any(|tree| {
            path.strip_prefix(tree.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        }) {
            continue;
        }
        if path.ends_with('/') {
            // An untracked nested repository. `git add` would record a bare
            // gitlink that restores nothing, so refuse rather than pretend.
            return Err(format!(
                "untracked nested repository '{path}' cannot be snapshotted; move or remove it \
                 before this worktree can be backed up"
            ));
        }
        to_add.push(path);
    }
    if !to_add.is_empty() {
        let mut stdin = to_add.join("\0");
        stdin.push('\0');
        git_env_stdin(
            worktree,
            &env,
            &["update-index", "--add", "-z", "--stdin"],
            stdin.as_bytes(),
        )
        .map_err(|e| format!("staging untracked files into the temporary index failed: {e}"))?;
    }

    let tree = git_env_stdout(worktree, &env, &["write-tree"])
        .map_err(|e| format!("write-tree failed: {e}"))?;
    let head_tree = git_stdout(worktree, &["rev-parse", &format!("{head}^{{tree}}")])
        .map_err(|e| format!("cannot resolve HEAD's tree: {e}"))?;
    if tree == head_tree {
        return Ok(head.to_string());
    }

    let message = format!(
        "loom-backup: {item} ({}) before idle-open worktree reap\n\nSnapshot of every tracked \
         change and untracked, non-ignored file in the worktree at {}, taken by the \
         loom-daemon worktree reaper before removing the idle worktree of an open item \
         (#12458). Parent is the worktree's HEAD; the branch itself is unchanged.\n",
        branch.unwrap_or("detached HEAD"),
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    git_env_stdout(
        worktree,
        &[
            ("GIT_INDEX_FILE", index.path_str()),
            ("GIT_AUTHOR_NAME", SNAPSHOT_AUTHOR),
            ("GIT_AUTHOR_EMAIL", SNAPSHOT_EMAIL),
            ("GIT_COMMITTER_NAME", SNAPSHOT_AUTHOR),
            ("GIT_COMMITTER_EMAIL", SNAPSHOT_EMAIL),
        ],
        &[
            "commit-tree",
            &tree,
            "-p",
            head,
            "--no-gpg-sign",
            "-m",
            &message,
        ],
    )
    .map_err(|e| format!("commit-tree failed: {e}"))
}

/// A temporary index file, removed on drop so a failed backup leaves no
/// residue behind.
struct TempIndex {
    path: PathBuf,
    path_str: String,
}

impl TempIndex {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "loom-backup-index-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        // `read-tree` creates the file; refuse a stale one rather than reuse it.
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                format!("cannot clear stale temporary index {}: {e}", path.display())
            })?;
        }
        let path_str = path
            .to_str()
            .ok_or_else(|| format!("temporary index path {} is not UTF-8", path.display()))?
            .to_string();
        Ok(Self { path, path_str })
    }

    fn path_str(&self) -> &str {
        &self.path_str
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // `git add` may leave an `<index>.lock` behind if killed mid-write.
        let _ = std::fs::remove_file(format!("{}.lock", self.path_str));
    }
}

/// Trimmed stdout of a successful local `git` command, else its stderr.
fn git_stdout(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_env_stdout(dir, &[], args)
}

fn git_env_stdout(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Result<String, String> {
    run_git_with_timeout(dir, args, env, LOCAL_GIT_TIMEOUT)
}

/// [`git_env_stdout`] with `stdin` fed to the command (NUL-separated path
/// lists for `update-index --stdin`).
fn git_env_stdin(
    dir: &Path,
    env: &[(&str, &str)],
    args: &[&str],
    stdin: &[u8],
) -> Result<String, String> {
    run_git(dir, args, env, Some(stdin), LOCAL_GIT_TIMEOUT)
}

/// Run `git <args>` in `dir`, killing it after `timeout`. Output is captured
/// through files, never pipes, so a chatty command cannot deadlock the poll
/// loop. `Ok(stdout)` trimmed on exit 0; `Err` names the exit status and the
/// tail of stderr otherwise.
fn run_git_with_timeout(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> Result<String, String> {
    run_git(dir, args, env, None, timeout)
}

fn run_git(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<String, String> {
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let out_path = std::env::temp_dir().join(format!("loom-backup-git-{stamp}.out"));
    let err_path = std::env::temp_dir().join(format!("loom-backup-git-{stamp}.err"));
    let result = run_git_capturing(dir, args, env, stdin, timeout, &out_path, &err_path);
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    result
}

fn run_git_capturing(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
    timeout: Duration,
    out_path: &Path,
    err_path: &Path,
) -> Result<String, String> {
    let out_file =
        std::fs::File::create(out_path).map_err(|e| format!("cannot create output file: {e}"))?;
    let err_file =
        std::fs::File::create(err_path).map_err(|e| format!("cannot create output file: {e}"))?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run git {}: {e}", args.first().unwrap_or(&"")))?;
    if let Some(bytes) = stdin {
        // The child's stdout/stderr are files, so it drains stdin as we write
        // and this cannot deadlock; the handle is dropped (EOF) before polling.
        if let Some(mut pipe) = child.stdin.take() {
            use std::io::Write as _;
            if let Err(e) = pipe.write_all(bytes) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not feed git {}: {e}", args.first().unwrap_or(&"")));
            }
        }
    }
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "git {} timed out after {}s",
                    args.first().unwrap_or(&""),
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => return Err(format!("could not poll git: {e}")),
        }
    };
    let stdout = std::fs::read_to_string(out_path).unwrap_or_default();
    if status.success() {
        return Ok(stdout.trim().to_string());
    }
    let stderr = std::fs::read_to_string(err_path).unwrap_or_default();
    let tail: String = stderr
        .trim()
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Err(if tail.is_empty() {
        format!("git {} exited with {status}", args.first().unwrap_or(&""))
    } else {
        format!("git {} exited with {status}: {tail}", args.first().unwrap_or(&""))
    })
}

#[cfg(test)]
#[path = "open_backup_tests.rs"]
mod tests;
