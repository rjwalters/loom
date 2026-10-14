//! Who owns a run dir, and whether that owner is still the process that wrote
//! the marker (issue #11031).
//!
//! [`super::OWNER_FILE`] records a pid. A pid alone is not a durable handle on
//! a process: once the owner exits, the number can be handed to an unrelated
//! process, and a bare `kill(pid, 0)` then reports the dead owner "alive" for
//! as long as the stranger runs. That kept dead sweeps' 24 to 26 GB run dirs
//! on disk (loom-worker-1, 2026-10-09).
//!
//! So [`super::provision`] also records the owner's **start identity** in
//! [`OWNER_START_FILE`]: on Linux the boot id plus `/proc/<pid>/stat`'s
//! `starttime` (clock ticks since boot, immune to wall-clock steps); on macOS
//! the kernel's recorded start time (`proc_pidinfo`). A live pid whose
//! identity differs from the recorded one is a different process.
//!
//! # Fail-safe direction
//!
//! The identity can only turn "alive" into "dead" on *positive* evidence: both
//! the recorded and the current identity are known and they differ. An
//! unreadable or missing identity (an older dir, a host with no probe, a
//! process this user may not inspect) keeps the bare liveness verdict, so an
//! unknown is always a keep.

use std::path::Path;

/// File inside a run dir recording the owner's start identity (see the module
/// doc). A sibling of [`super::OWNER_FILE`] rather than a second line in it,
/// so a binary that predates it still parses the marker.
pub const OWNER_START_FILE: &str = ".loom-run-owner-start";

/// The current start identity of `pid`, when this host can derive it.
#[must_use]
pub fn process_start_token(pid: u32) -> Option<String> {
    platform_start_token(pid)
}

#[cfg(target_os = "linux")]
fn platform_start_token(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_ppid, start_ticks) = crate::orphan_process_reaper::parse_stat(&stat)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    Some(format!("linux:{}:{start_ticks}", boot.trim()))
}

#[cfg(target_os = "macos")]
fn platform_start_token(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `proc_bsdinfo` is plain old data, so all-zero is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable `proc_bsdinfo` of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    (written == size)
        .then(|| format!("darwin:{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_start_token(_pid: u32) -> Option<String> {
    None
}

/// Record `owner_pid`'s start identity in `dir`. Best-effort: with no
/// identity the dir falls back to bare pid liveness.
pub fn record_start_token(dir: &Path, owner_pid: u32) {
    if let Some(token) = process_start_token(owner_pid) {
        let _ = std::fs::write(dir.join(OWNER_START_FILE), format!("{token}\n"));
    }
}

/// The start identity recorded in `dir`, if any.
#[must_use]
pub fn recorded_start_token(dir: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(dir.join(OWNER_START_FILE)).ok()?;
    if !meta.is_file() {
        return None;
    }
    let token = std::fs::read_to_string(dir.join(OWNER_START_FILE)).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// The recorded owner pid of `dir` when that owner is still running, `None`
/// when it is gone, when there is no owner recorded, or when the pid now
/// belongs to a different process (its start identity differs from the one
/// recorded). `alive` and `identity` are injected for tests.
pub fn running_owner_with(
    dir: &Path,
    alive: &dyn Fn(u32) -> bool,
    identity: &dyn Fn(u32) -> Option<String>,
) -> Option<u32> {
    let pid = super::owner_pid(dir)?;
    if !alive(pid) {
        return None;
    }
    match (recorded_start_token(dir), identity(pid)) {
        (Some(recorded), Some(current)) if recorded != current => {
            log::info!(
                "run_target_dir: owner pid {pid} of {} is alive but is a different process \
                 (started {current}, owner started {recorded}): the pid was reused, the owner \
                 is gone",
                dir.display()
            );
            None
        }
        _ => Some(pid),
    }
}

/// [`running_owner_with`] with the production probes.
#[must_use]
pub fn running_owner(dir: &Path) -> Option<u32> {
    running_owner_with(dir, &crate::live_claim::pid_is_live_process, &process_start_token)
}

/// Remove a run dir with its owner files LAST: every other child first, then
/// the dir itself. A removal cut short (a daemon stopped mid-way, an I/O
/// error) leaves a dir that still carries its marker, so the orphan sweep
/// finishes it instead of keeping an unmarked dir forever.
pub fn remove_marker_last(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == super::OWNER_FILE || name == OWNER_START_FILE {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    std::fs::remove_dir_all(dir)
}
