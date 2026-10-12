//! Remove a daemon sweep's run dir when the sweep ends (issue #11031).
//!
//! A role-runner tick removes its own dir at run end ([`super::RunDirGuard`]).
//! A daemon sweep had nothing equivalent: `worker_spawn` derives the dir
//! (`<role>-<pid>-<ts>`) and then `exec()`s, so nothing in the spawn chain
//! survives to clean up, and the dir waited for the orphan sweep. On
//! loom-worker-1 (2026-10-09) that was about 50 GB of finished sweeps' dirs.
//!
//! The registry does see the end: the reaper's death path (a completed or
//! failed sweep) and `finish_cancel` (an operator cancel or a watchdog
//! auto-cancel, which runs before the watchdog's re-dispatch). Both call
//! [`reclaim_at_sweep_end`] with the sweep's pid and process group. The death
//! path calls it only once the dead leader's process group has drained
//! (#11076): until then the sweep is still live and its dir is in use.
//!
//! # Which dirs
//!
//! `exec()` keeps the pid along the whole spawn chain (`spawn-worker.sh` ->
//! `loom-daemon spawn-worker` -> the harness), so the pid the registry tracks
//! is the pid `provision` wrote into the marker. [`dirs_owned_by`] finds the
//! direct children of `<repo>/.loom/targets` whose marker names that pid.
//! Nothing has to be planned at dispatch or stored on the entry, and a dir
//! derived by an older binary is found the same way.
//!
//! # Gates (an unknown is a keep)
//!
//! The `.loom/targets` root is vetted exactly as the orphan sweep vets it
//! (a real directory chain inside the repo, never a symlink). Then, per dir:
//! the sweep's process group has no live member, the recorded owner is not
//! running ([`super::owner::running_owner`], which sees through pid reuse),
//! and no process holds anything open under the dir. A dir kept here is left
//! to [`crate::target_orphan_reclaim`], whose dead-owner tier collects it a
//! few minutes later.
//!
//! The removal runs on a detached thread, never under the registry lock: a
//! 26 GB tree takes seconds to unlink, and the group may need a moment to
//! drain after a cancel's SIGKILL.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Removal;

/// How long the reclaim thread waits for a sweep's process group to drain
/// before leaving the dir to the orphan sweep. Covers a cancel whose SIGKILL
/// has only just been sent. The reaper's death path has already waited for
/// the group (#11076), so there this returns at once unless that wait gave up.
pub const SWEEP_END_GROUP_GRACE: Duration = Duration::from_secs(120);

/// Every run dir directly under `<repo_root>/.loom/targets` whose owner marker
/// names `pid`. Empty when the root is missing or fails [`vet`]: a symlinked
/// `.loom/targets` holds somebody else's directories.
///
/// [`vet`]: crate::target_orphan_reclaim::vet_path_inside
#[must_use]
pub fn dirs_owned_by(repo_root: &Path, pid: u32) -> Vec<PathBuf> {
    let root = super::targets_root(repo_root);
    if pid == 0 || crate::target_orphan_reclaim::vet_path_inside(repo_root, &root).is_err() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| super::is_run_target_dir(path))
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()))
        .filter(|path| {
            std::fs::symlink_metadata(path.join(super::OWNER_FILE)).is_ok_and(|m| m.is_file())
        })
        .filter(|path| super::owner_pid(path) == Some(pid))
        .collect()
}

/// The liveness inputs and the remover, injected so every gate is testable.
pub struct SweepEndProbes<'a> {
    /// Whether the sweep's process group still has a member.
    pub group_alive: &'a dyn Fn(u32) -> bool,
    /// Bare pid liveness for the recorded owner.
    pub owner_alive: &'a dyn Fn(u32) -> bool,
    /// The current start identity of a pid ([`super::owner::process_start_token`]).
    pub identity: &'a dyn Fn(u32) -> Option<String>,
    /// `Some(true)` held open, `Some(false)` free, `None` could not probe.
    pub open_handles: &'a dyn Fn(&Path) -> Option<bool>,
    pub remove: &'a dyn Fn(&Path) -> std::io::Result<()>,
}

/// Run every gate on one dir of an ended sweep and remove it when all pass.
pub fn reclaim_with(dir: &Path, pgid: Option<u32>, probes: &SweepEndProbes<'_>) -> Removal {
    if let Some(pgid) = pgid.filter(|&g| (probes.group_alive)(g)) {
        return Removal::Kept(format!("process group {pgid} still has a live member"));
    }
    if let Some(pid) = super::owner::running_owner_with(dir, probes.owner_alive, probes.identity) {
        return Removal::Kept(format!("owner pid {pid} is still running"));
    }
    match (probes.open_handles)(dir) {
        Some(false) => match (probes.remove)(dir) {
            Ok(()) => Removal::Removed,
            Err(e) => Removal::Failed(e.to_string()),
        },
        Some(true) => Removal::Kept("a process holds something open under it".to_string()),
        None => Removal::Kept("the open-handle probe could not run".to_string()),
    }
}

/// Wait (up to `grace`) for `pgid` to drain. `true` once it has.
fn wait_for_group(pgid: Option<u32>, grace: Duration) -> bool {
    let Some(pgid) = pgid else { return true };
    let deadline = Instant::now() + grace;
    loop {
        if !super::process_group_alive(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Remove the run dir(s) of the sweep `sweep_id` that just ended, on a
/// detached thread. Returns `None` (and spawns nothing) when the sweep left no
/// run dir, which is every sweep in a repo that is not a Cargo workspace or
/// that sets its own `CARGO_TARGET_DIR`.
pub fn reclaim_at_sweep_end(
    repo_root: &Path,
    sweep_id: &str,
    pid: u32,
    pgid: Option<u32>,
) -> Option<std::thread::JoinHandle<()>> {
    let dirs = dirs_owned_by(repo_root, pid);
    if dirs.is_empty() {
        return None;
    }
    let owned_id = sweep_id.to_string();
    let spawned = std::thread::Builder::new()
        .name("run-dir-sweep-end".to_string())
        .spawn(move || {
            wait_for_group(pgid, SWEEP_END_GROUP_GRACE);
            let probes = SweepEndProbes {
                group_alive: &super::process_group_alive,
                owner_alive: &crate::live_claim::pid_is_live_process,
                identity: &super::owner::process_start_token,
                open_handles: &crate::target_orphan_reclaim::production_open_handles,
                remove: &super::owner::remove_marker_last,
            };
            for dir in dirs {
                let size = crate::target_orphan_reclaim::newest_mtime_and_size(&dir)
                    .map_or(0, |(_, bytes)| bytes);
                log_outcome(&owned_id, &dir, size, &reclaim_with(&dir, pgid, &probes));
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::warn!(
                "run_target_dir: could not start the sweep-end reclaim for {sweep_id}: {e}; the \
                 orphan sweep will collect its run dir"
            );
            None
        }
    }
}

fn log_outcome(sweep_id: &str, dir: &Path, size: u64, outcome: &Removal) {
    let human = crate::tmpfs_reclaim::human_size(size);
    match outcome {
        Removal::Removed => log::info!(
            "run_target_dir: category=sweep_end_run_dir removed {} ({human}, {size} bytes) at \
             the end of sweep {sweep_id}",
            dir.display()
        ),
        Removal::Absent => {}
        Removal::Kept(why) => log::info!(
            "run_target_dir: kept {} at the end of sweep {sweep_id} ({why}); the orphan sweep \
             will collect it once its owner is gone",
            dir.display()
        ),
        Removal::Failed(why) => log::warn!(
            "run_target_dir: could not remove {} at the end of sweep {sweep_id}: {why}",
            dir.display()
        ),
    }
}
