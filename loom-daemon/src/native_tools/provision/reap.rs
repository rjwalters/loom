//! Reaping per-launch native harness state (#8663).
//!
//! # What leaks, and why a reaper is needed at all
//!
//! Every guarded native launch gets a fresh `uuid`-named session directory
//! under `~/.local/state/loom/native-tools/<workspace>/` ([`super::state`]),
//! holding the XDG trees the harness writes its cache, sessions and auth into.
//! Nothing removed them: three fleet hosts held 252–358 session directories
//! (30–37 GB) after a day of native-lane operation, and one filled its root
//! volume twice.
//!
//! [`super::shared`] takes the ~126 MB plugin tree out of the per-session
//! directory, which is the bulk of the growth. What is left still has to be
//! reaped, because a session directory is never empty and never reused.
//!
//! # Why launch-time, not exit-time
//!
//! There is no exit-time hook to use. `worker_spawn::exec` **replaces** this
//! process image with the harness CLI (`execve`), so no parent survives the
//! session to clean up after it, and the CLI itself is an upstream binary with
//! no Loom-owned shutdown path. The reap therefore runs where a Loom process
//! demonstrably exists: at the start of the next launch for the same workspace
//! ([`super::state::create`]), plus the operator-driven `loom-daemon clean`
//! pass over every workspace.
//!
//! # What "stale" means
//!
//! A session directory is removed only when it can be shown to belong to no
//! live launch:
//!
//! * **This host, live pid** — kept, whatever its age. The pid recorded in
//!   [`SESSION_RECORD`] is the harness process's own (`execve` preserves it),
//!   so a 12-hour sweep is never reaped out from under itself.
//! * **This host, exited pid** — removed once [`Policy::exited_min_age`] has
//!   passed, which covers the window where a peer has created the directory
//!   but not yet written its record.
//! * **Another host** (a shared `$HOME`; the record names the writer) or **no
//!   record at all** (written by a daemon older than #8663) — pid liveness is
//!   unanswerable, so only age decides: [`Policy::orphan_max_age`].
//! * **Unreadable age** — kept. Never reap what cannot be aged.
//!
//! Shared binding trees ([`BINDINGS`]) are content-keyed, so a plugin-pin bump
//! strands the previous one; each carries a [`LAST_USED`] marker and is removed
//! after [`Policy::binding_max_idle`] without a launch.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Per-workspace directory holding shared, content-keyed binding trees.
pub(crate) const BINDINGS: &str = "bindings";

/// Per-workspace directory holding binding trees that have not yet been
/// renamed into [`BINDINGS`]. A crash mid-provision strands one here.
pub(crate) const STAGING: &str = ".staging";

/// The liveness record a launch writes into its own session directory.
pub(crate) const SESSION_RECORD: &str = "loom-session.json";

/// Marker file inside a binding tree whose mtime is the last launch that used
/// it. A directory mtime cannot serve: reading a tree does not bump it.
pub(crate) const LAST_USED: &str = ".last-used";

/// Age thresholds for [`reap_base`] / [`reap_workspace`].
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Minimum age before a session whose recorded pid has exited is removed.
    pub exited_min_age: Duration,
    /// Age at which a session with no usable liveness record is removed.
    pub orphan_max_age: Duration,
    /// Idle time after which a shared binding tree is removed.
    pub binding_max_idle: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            exited_min_age: Duration::from_secs(15 * 60),
            orphan_max_age: Duration::from_secs(6 * 60 * 60),
            binding_max_idle: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

/// What a reap pass found and (unless `dry_run`) removed.
#[derive(Debug, Default)]
pub struct Report {
    /// Session directories judged stale.
    pub sessions: Vec<PathBuf>,
    /// Shared binding trees judged idle, plus stranded staging trees.
    pub bindings: Vec<PathBuf>,
    /// Session directories kept because a live launch owns them.
    pub kept: usize,
    /// Bytes accounted for by everything in `sessions` + `bindings`.
    pub bytes: u64,
    /// Diagnostics; a reap failure never fails the launch that triggered it.
    pub errors: Vec<String>,
}

impl Report {
    /// Total number of directories this pass named.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.sessions.len() + self.bindings.len()
    }

    /// Whether the pass named nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.removed() == 0
    }

    fn absorb(&mut self, other: Self) {
        self.sessions.extend(other.sessions);
        self.bindings.extend(other.bindings);
        self.kept += other.kept;
        self.bytes += other.bytes;
        self.errors.extend(other.errors);
    }
}

/// The liveness record written into every session directory.
#[derive(Debug, Serialize, Deserialize)]
struct SessionRecord {
    schema: u32,
    /// The harness process's pid — `execve` preserves it across the hand-off.
    pid: u32,
    /// The host that wrote the record; a pid from another host says nothing.
    host: String,
    /// Creation time (unix seconds), for diagnostics.
    created: u64,
}

/// Write the liveness record for a freshly created session directory.
///
/// # Errors
///
/// Propagates a filesystem failure. The caller treats this as fatal: a session
/// with no record is only reapable on age, which is exactly the behaviour
/// #8663 is removing.
pub(crate) fn record_session(directory: &Path) -> Result<()> {
    let record = SessionRecord {
        schema: 1,
        pid: std::process::id(),
        host: crate::watchdog::escalate::hostname(),
        created: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
    };
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .context("cannot stage the native session record")?;
    file.write_all(&serde_json::to_vec(&record)?)?;
    file.persist(directory.join(SESSION_RECORD))
        .map_err(|error| error.error)
        .context("cannot write the native session record")?;
    Ok(())
}

/// Refresh a binding tree's [`LAST_USED`] marker. Best effort: a tree that
/// cannot be marked is merely reapable sooner, never wrong.
pub(crate) fn mark_used(entry: &Path) {
    let _ = fs::write(entry.join(LAST_USED), b"");
}

/// The host-wide native state base: `LOOM_NATIVE_TOOLS_DIR` when set (the
/// container layer points it at the ephemeral root), else the same
/// `~/.local/state/loom/native-tools` [`super::state::create`] defaults to.
#[must_use]
pub fn default_base() -> Option<PathBuf> {
    std::env::var_os("LOOM_NATIVE_TOOLS_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state/loom/native-tools")))
}

/// Reap every workspace under `base`.
#[must_use]
pub fn reap_base(base: &Path, policy: &Policy, dry_run: bool) -> Report {
    let mut report = Report::default();
    let entries = match fs::read_dir(base) {
        Ok(entries) => entries,
        // A base that does not exist yet is not an error: nothing has leaked.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return report,
        Err(error) => {
            report
                .errors
                .push(format!("cannot read {}: {error}", base.display()));
            return report;
        }
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            report.absorb(reap_workspace(&entry.path(), policy, dry_run));
        }
    }
    report
}

/// Reap one workspace's session directories, stranded staging trees and idle
/// binding trees.
#[must_use]
pub(crate) fn reap_workspace(workspace: &Path, policy: &Policy, dry_run: bool) -> Report {
    let mut report = Report::default();
    let host = crate::watchdog::escalate::hostname();
    let now = SystemTime::now();
    let entries = match fs::read_dir(workspace) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return report,
        Err(error) => {
            report
                .errors
                .push(format!("cannot read {}: {error}", workspace.display()));
            return report;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        if name == BINDINGS {
            reap_bindings(&path, policy, now, dry_run, &mut report);
        } else if name == STAGING {
            // A staged tree is nobody's live state by construction: it is
            // published by `rename`, so anything still here lost its creator.
            for staged in fs::read_dir(&path).into_iter().flatten().flatten() {
                if age(&staged.path(), now).is_some_and(|age| age >= policy.orphan_max_age) {
                    remove(&staged.path(), dry_run, Kind::Binding, &mut report);
                }
            }
        } else if uuid::Uuid::parse_str(&name).is_ok() {
            if stale_session(&path, policy, &host, now) {
                remove(&path, dry_run, Kind::Session, &mut report);
            } else {
                report.kept += 1;
            }
        }
    }
    report
}

fn reap_bindings(
    bindings: &Path,
    policy: &Policy,
    now: SystemTime,
    dry_run: bool,
    report: &mut Report,
) {
    for entry in fs::read_dir(bindings).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let marker = path.join(LAST_USED);
        let idle = if marker.exists() {
            age(&marker, now)
        } else {
            age(&path, now)
        };
        if idle.is_some_and(|idle| idle >= policy.binding_max_idle) {
            remove(&path, dry_run, Kind::Binding, report);
        }
    }
}

/// Whether `directory` belongs to no live launch. Fails safe: anything
/// unreadable or unaged is kept.
fn stale_session(directory: &Path, policy: &Policy, host: &str, now: SystemTime) -> bool {
    let Some(age) = newest_activity(directory, now) else {
        return false;
    };
    match read_record(directory) {
        Some(record) if record.host == host => {
            !pid_alive(record.pid) && age >= policy.exited_min_age
        }
        // Another host's pid is not a liveness answer, and neither is no
        // record at all (pre-#8663). Age is all that is left.
        _ => age >= policy.orphan_max_age,
    }
}

fn read_record(directory: &Path) -> Option<SessionRecord> {
    let raw = fs::read(directory.join(SESSION_RECORD)).ok()?;
    serde_json::from_slice(&raw).ok()
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs no action beyond an existence/permission check.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    // EPERM means the process exists and belongs to another user — alive.
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// Age of the most recent write anywhere in `directory`'s top level (the
/// directory itself or one of its immediate children). A harness writes into
/// subdirectories, which never bumps the session directory's own mtime, so the
/// bare directory mtime would age a busy session as if it were idle.
fn newest_activity(directory: &Path, now: SystemTime) -> Option<Duration> {
    let mut newest = modified(directory)?;
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        if let Some(time) = modified(&entry.path()) {
            newest = newest.max(time);
        }
    }
    now.duration_since(newest).ok()
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok()?.modified().ok()
}

fn age(path: &Path, now: SystemTime) -> Option<Duration> {
    now.duration_since(modified(path)?).ok()
}

/// Which tally in a [`Report`] a removal belongs to.
#[derive(Clone, Copy)]
enum Kind {
    Session,
    Binding,
}

/// Record `path` under `kind`, adding its size, and remove it unless `dry_run`.
fn remove(path: &Path, dry_run: bool, kind: Kind, report: &mut Report) {
    let bytes = tree_size(path);
    if !dry_run {
        if let Err(error) = fs::remove_dir_all(path) {
            report
                .errors
                .push(format!("cannot remove {}: {error}", path.display()));
            return;
        }
    }
    match kind {
        Kind::Session => report.sessions.push(path.to_path_buf()),
        Kind::Binding => report.bindings.push(path.to_path_buf()),
    }
    report.bytes += bytes;
}

/// Bytes held by a tree. Best effort — an unreadable entry contributes zero
/// rather than aborting a pass whose purpose is to free space.
fn tree_size(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| tree_size(&entry.path()))
        .sum::<u64>()
        + metadata.len()
}

/// Render a report as the one-line summary `clean` and the launch path print.
#[must_use]
pub fn summary(report: &Report, dry_run: bool) -> String {
    let verb = if dry_run { "Would remove" } else { "Removed" };
    format!(
        "{verb} {} stale native session director{} and {} idle binding tree{} ({}); {} live session{} kept",
        report.sessions.len(),
        if report.sessions.len() == 1 { "y" } else { "ies" },
        report.bindings.len(),
        if report.bindings.len() == 1 { "" } else { "s" },
        human_bytes(report.bytes),
        report.kept,
        if report.kept == 1 { "" } else { "s" },
    )
}

fn human_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    for (unit, scale) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if bytes >= scale {
            return format!("{:.1}{unit}", value / scale as f64);
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
#[path = "reap_tests.rs"]
mod tests;
