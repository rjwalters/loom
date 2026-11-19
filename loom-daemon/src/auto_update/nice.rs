//! Low-priority (`nice`) setup for the rebuild subprocess (split out of
//! `auto_update.rs` to keep it under the file-size budget, #11042).

use std::process::Command;

/// `nice` value applied to the rebuild subprocess when it runs under the gate-4
/// deadline override, so a build forced onto a saturated host yields CPU to the
/// in-flight sweeps instead of competing with them. `19` is the maximum (lowest
/// priority) niceness on Linux and macOS.
pub(super) const LOW_PRIORITY_NICE: i32 = 19;

/// Nice the child (and, by inheritance, the `cargo`/`rustc` processes it
/// spawns) down to [`LOW_PRIORITY_NICE`] before `exec`. Best-effort: a failing
/// `setpriority` is deliberately ignored — a build at normal priority is far
/// better than no build at all, which is the starvation this whole path exists
/// to end (#4929).
#[cfg(unix)]
pub(super) fn nice_child(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `pre_exec` runs between fork and exec, where only
    // async-signal-safe work is permitted. `setpriority(2)` is a bare syscall
    // wrapper — it allocates nothing, takes no locks, and touches no libc
    // global state — so it is safe in that window.
    unsafe {
        command.pre_exec(|| {
            libc::setpriority(libc::PRIO_PROCESS, 0, LOW_PRIORITY_NICE);
            Ok(())
        });
    }
}

/// Non-unix hosts have no `setpriority`; the build simply runs at normal
/// priority (the daemon's supervised install targets are macOS/Linux).
#[cfg(not(unix))]
pub(super) fn nice_child(_command: &mut Command) {}
