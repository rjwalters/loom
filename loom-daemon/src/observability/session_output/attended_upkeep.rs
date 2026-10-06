//! **Housekeeping for attended runs** (#10125): the start diagnostic that
//! outlives its stderr line, and the state files a failed or killed tailer
//! leaves behind.
//!
//! # The last start outcome
//!
//! `worktree.sh` discards `lease ensure`'s stderr, so the one line that says
//! why an issue's live log stays empty (not configured, transcript not
//! located, already running) used to reach nobody on the main claim path.
//! [`record_start`] also writes it to `last-start.log` in the attended state
//! directory, overwriting the previous one. The line is
//! [`Outcome::describe`](super::Outcome::describe)'s text, which never holds a
//! credential: the ingest key is read only inside the detached tailer.
//!
//! # Stale state files
//!
//! A tailer removes its own lock, claim and queue files when its run ends. A
//! tailer that is SIGKILLed cannot, and a start whose spawn failed left the
//! lock file it probed. [`sweep_stale`] runs at every configured start and
//! removes, across every transcript's files:
//!
//! - a `<stream>.lock` whose `flock` is free. It is taken first and removed
//!   while held, the same way a finishing tailer releases it, so a file a
//!   live tailer holds is never touched;
//! - a `<stream>.<pid>.otlp-<n>.jsonl` queue whose tailer process is gone;
//! - a `<stream>.claim` whose lock is free and that is older than any tailer
//!   lives ([`super::DEFAULT_MAX_AGE_SECS`]), so a claim a just-spawned tailer
//!   is about to use is never removed.
//!
//! The directory itself stays: it is gitignored with the rest of
//! `.loom/logs/`, and it holds `last-start.log`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{state_dir, Outcome, StreamLock, DEFAULT_MAX_AGE_SECS};

/// The file [`record_start`] writes, inside the attended state directory.
pub const LAST_START_FILE: &str = "last-start.log";

/// Where `workspace`'s last start outcome is recorded, when `workspace` is
/// inside a Loom checkout.
#[must_use]
pub fn last_start_path(workspace: &Path) -> Option<PathBuf> {
    let root = crate::repo_root::resolve_repo_root(&workspace.to_string_lossy()).ok()?;
    Some(state_dir(&root).join(LAST_START_FILE))
}

/// Record one start's outcome as `<UTC time> <diagnostic>`, replacing the
/// previous record. Best effort: a failure here never affects the claim.
pub fn record_start(workspace: &Path, issue: u32, outcome: &Outcome) {
    let Some(path) = last_start_path(workspace) else {
        return;
    };
    let line = format!(
        "{} {}\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        outcome.describe(issue)
    );
    let _ = write_atomically(&path, line.as_bytes());
}

/// Write `bytes` to `path` through a per-process staging file, so a reader
/// never sees a half-written line and two starts never interleave.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension(format!("log.{}.tmp", std::process::id()));
    std::fs::write(&staging, bytes)?;
    std::fs::rename(&staging, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staging);
    })
}

/// Remove the stale lock, queue and claim files in `dir`. Never removes a
/// file a running tailer still uses.
pub fn sweep_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let claim_age = Duration::from_secs(DEFAULT_MAX_AGE_SECS);
    for name in &names {
        let path = dir.join(name);
        if let Some(stream) = name.strip_suffix(".claim") {
            // The lock is held through the removal, so no tailer of this
            // transcript can start reading the claim meanwhile.
            let lock = StreamLock::try_acquire(&dir.join(format!("{stream}.lock")));
            if let Ok(Some(lock)) = lock {
                if older_than(&path, claim_age) {
                    let _ = std::fs::remove_file(&path);
                }
                lock.release();
            }
        } else if name.ends_with(".lock") {
            if let Ok(Some(lock)) = StreamLock::try_acquire(&path) {
                lock.release();
            }
        } else if dead_tailers_queue(name) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether `name` is a `<stream>.<pid>.otlp-<n>.jsonl` queue whose tailer
/// process is no longer running.
fn dead_tailers_queue(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let mut parts = stem.rsplit('.');
    let (Some(otlp), Some(pid)) = (parts.next(), parts.next()) else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    otlp.strip_prefix("otlp-")
        .is_some_and(|n| n.parse::<u32>().is_ok())
        && pid != std::process::id()
        && !crate::live_claim::pid_is_live_process(pid)
}

/// Whether `path` was last modified more than `age` ago.
fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|since| since > age)
}
