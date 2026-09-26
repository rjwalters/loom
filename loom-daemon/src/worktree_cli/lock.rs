//! The repo-global worktree-add lock (#8195, epic #7810 slice 1).
//!
//! # Why this first
//!
//! `worktree.sh` performs 32 irreversible operations and has taken 26
//! fix-commits in six months, three of them data-loss classes. This lock is
//! what every one of those destructive paths is supposed to be standing
//! behind, and it has been wrong before: #6014/#6017 — *"narrow worktree-add
//! lock scope and verify release ownership"*.
//!
//! It is also the right size and shape to go first: ~135 shell lines, no forge
//! I/O, and a dedicated retained suite (`test-worktree-concurrency.sh`).
//!
//! # The contract, and the incident behind each part
//!
//! - **`mkdir` is the acquisition primitive.** It is atomic on every
//!   filesystem Loom runs on; a check-then-create would not be.
//! - **The lock is repo-GLOBAL, not per-issue.** `git worktree add` mutates
//!   `.git/worktrees/` for the whole repository, so two adds for different
//!   issues still race. The issue number is recorded as owner metadata only.
//! - **Release is token-verified (#6014).** A holder that has already been
//!   cleared and whose lock has been REASSIGNED must not delete the new
//!   holder's lock. Release therefore removes the directory only when the
//!   token it was handed still matches the one recorded inside. An empty
//!   token releases nothing: a caller that cannot prove ownership must never
//!   remove a lock, because the thing it would remove is someone else's.
//! - **Stale-PID recovery happens exactly once per acquisition.** A lock whose
//!   recorded owner is dead is broken and retried; but only one retry, so two
//!   processes racing to break the same stale lock cannot livelock.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What `owner.json` carries. Field names are contract: the retained suite and
/// `merge-pr.sh`'s diagnostics both read this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    // Every field defaults. The shell read this file with `awk`, one field at
    // a time, so a partial or older-format owner.json still yielded whatever
    // it did contain. Serde's all-or-nothing default made the port STRICTER in
    // a way that silently lost function: a lock written without a `token`
    // (the retained suite writes exactly that, and so did every lock taken
    // before #6014) failed to deserialise, so `owner_pid` was never read —
    // which killed BOTH the holder-PID diagnostic and stale-lock recovery. A
    // lock whose owner is dead then never got reclaimed and every acquisition
    // timed out.
    #[serde(default)]
    pub issue: u32,
    #[serde(default)]
    pub owner_pid: u32,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub script: String,
    #[serde(default)]
    pub acquired_at: String,
}

/// Why an acquisition failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// The deadline passed. Carries the holder's pid when one was readable, so
    /// the caller can name it — the suite asserts the pid appears in the
    /// timeout output.
    Timeout { holder_pid: Option<u32> },
    /// The lock directory's parent could not be created.
    Unusable(String),
}

/// The lock directory for this repository.
///
/// Derived from `git rev-parse --git-common-dir` so every linked worktree of
/// the same repository resolves to the SAME lock — which is the entire point
/// of a repo-global lock, and would silently not hold if this used
/// `--git-dir` (per-worktree) instead.
#[must_use]
pub fn locks_dir(repo: &Path) -> PathBuf {
    let common = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    match common {
        Some(c) => {
            // `--git-common-dir` may come back relative to the repo.
            let abs = if Path::new(&c).is_absolute() {
                PathBuf::from(&c)
            } else {
                repo.join(&c)
            };
            let abs = abs.canonicalize().unwrap_or(abs);
            abs.parent()
                .unwrap_or(Path::new("."))
                .join(".loom")
                .join("locks")
        }
        None => PathBuf::from(".loom/locks"),
    }
}

/// The lock itself. One per repository, named for the operation rather than
/// the issue — see the module docs.
#[must_use]
pub fn lock_path(repo: &Path) -> PathBuf {
    locks_dir(repo).join("worktree-add")
}

/// A one-shot acquisition token: pid, a high-resolution timestamp, and
/// randomness. All three, because two acquisitions by the same pid within the
/// same clock tick must still differ — the suite explicitly asserts that two
/// tokens are not identical before testing that a mismatched one is refused.
fn mint_token(pid: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{pid}-{nanos}-{}", rand_suffix())
}

fn rand_suffix() -> u32 {
    // Address-derived entropy; this needs to be distinct, not unpredictable.
    let x = Box::new(0u8);
    let a = std::ptr::addr_of!(*x) as usize;
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as usize);
    ((a ^ t) & 0x7fff) as u32
}

fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The pid recorded in a lock, if the file is readable and parseable.
fn recorded(lock: &Path) -> Option<Owner> {
    let text = std::fs::read_to_string(lock.join("owner.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether `pid` is alive. A failure to signal for any reason other than
/// "no such process" is treated as ALIVE — refusing to break a lock we cannot
/// prove is dead is the safe direction.
///
/// `pub(crate)` (not private) so [`super::issue_lock`] can reuse the same
/// liveness primitive for the DIFFERENT, per-issue claim lock it reads
/// (#8553) — one liveness check, not two that could drift.
pub(crate) fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Safety: `kill(pid, 0)` performs no action beyond permission/existence
    // checking.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Take the lock on behalf of `owner_pid`, or fail when `timeout` elapses.
///
/// # Why the owner pid is a PARAMETER
///
/// A lock is only a lock for as long as some process is alive to hold it, and
/// `acquire`'s stale-PID recovery reclaims one whose owner has died. In the
/// shell that owner was `$$` — `worktree.sh` itself, alive for the whole
/// critical section.
///
/// Porting the acquisition into a subcommand breaks that by construction: the
/// CLI process exits the moment it prints the token, so recording
/// `std::process::id()` writes a lock owned by an already-dead pid, and the
/// very next `acquire` reclaims it as stale. Two callers then hold it at once,
/// which is the condition the lock exists to prevent — and it looks like it
/// works, because acquisition always succeeds.
///
/// Caught by driving the CLI twice in a row rather than by the unit tests,
/// which hold the lock inside one live test process and so cannot see it. The
/// caller passes its OWN pid.
///
/// # Errors
/// [`AcquireError::Timeout`] when the deadline passes, carrying the holder's
/// pid when readable; [`AcquireError::Unusable`] when the locks directory
/// cannot be created.
pub fn acquire(
    repo: &Path,
    issue: u32,
    owner_pid: u32,
    timeout: Duration,
    poll: Duration,
) -> Result<String, AcquireError> {
    let dir = locks_dir(repo);
    std::fs::create_dir_all(&dir).map_err(|e| AcquireError::Unusable(e.to_string()))?;
    let lock = lock_path(repo);

    let started = Instant::now();
    let mut stale_retry_done = false;

    loop {
        // `create_dir` (not `create_dir_all`) is the atomic primitive: it
        // fails when the directory already exists, which is what makes it a
        // lock at all.
        if std::fs::create_dir(&lock).is_ok() {
            let pid = owner_pid;
            let token = mint_token(pid);
            let owner = Owner {
                issue,
                owner_pid: pid,
                token: token.clone(),
                script: "worktree.sh".to_string(),
                acquired_at: iso_now(),
            };
            // Best-effort, exactly as the shell's heredoc was: a lock whose
            // metadata could not be written is still HELD. Failing here would
            // leave the directory behind with no owner and no releaser.
            if let Ok(json) = serde_json::to_string_pretty(&owner) {
                let _ = std::fs::write(lock.join("owner.json"), json + "\n");
            }
            return Ok(token);
        }

        let holder = recorded(&lock);
        // 0 is serde's default, i.e. "the file did not say" — not a pid.
        let holder_pid = holder.as_ref().map(|o| o.owner_pid).filter(|p| *p != 0);

        // Stale-lock recovery, ONCE. Two processes racing to break the same
        // dead lock must not livelock breaking each other's fresh one.
        if let Some(pid) = holder_pid {
            if !stale_retry_done && !pid_alive(pid) {
                let _ = std::fs::remove_dir_all(&lock);
                stale_retry_done = true;
                continue;
            }
        }

        if started.elapsed() >= timeout {
            return Err(AcquireError::Timeout { holder_pid });
        }
        std::thread::sleep(poll);
    }
}

/// Release the lock, but ONLY if `token` still matches what the lock records.
///
/// Always succeeds from the caller's point of view: releasing a lock you no
/// longer own is not an error, it is a no-op, and that is the whole design.
/// An empty token releases nothing.
pub fn release(repo: &Path, token: &str) {
    if token.is_empty() {
        return;
    }
    let lock = lock_path(repo);
    if !lock.is_dir() {
        return;
    }
    // #6014: our acquisition may already have been cleared and the directory
    // REASSIGNED to a live holder. Removing it then would delete a lock we do
    // not own, which is the race this check exists for.
    match recorded(&lock) {
        Some(o) if o.token == token => {
            let _ = std::fs::remove_dir_all(&lock);
        }
        // A lock with unreadable or absent metadata is NOT ours to remove
        // either: we cannot prove it, so we leave it. The stale-PID path in
        // `acquire` is what reclaims a genuinely abandoned one.
        _ => {}
    }
}

#[cfg(test)]
mod tests;
