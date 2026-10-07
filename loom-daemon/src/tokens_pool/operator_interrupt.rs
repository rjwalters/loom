//! Ctrl-C for operator `accounts session` commands (issue #10661, item 5).
//!
//! Every `docker` call runs through [`crate::proc_exec`], which puts the
//! child in its own process group so a deadline can kill its descendants
//! (#10453). That group is not the terminal's foreground group, so a Ctrl-C
//! at the terminal no longer reaches an in-flight `docker run` or pull: only
//! the `loom-daemon` CLI got it, and it died leaving `docker` running.
//!
//! An operator command calls [`install`] first. Its SIGINT/SIGTERM handler
//! only records the signal; [`super::docker_cli`] forwards it to the running
//! `docker` child's group, waits briefly for it to exit, and fails the call
//! with [`Interrupted`], and refuses to start any further `docker` call. The
//! command then exits with an error, as Ctrl-C used to make it do. A second
//! Ctrl-C (or SIGTERM) SIGKILLs the `docker` group in flight and kills the
//! command at once, the default action.
//!
//! **The daemon never installs it.** [`pending`] is `None` until [`install`]
//! has run in this process, so the reconcile pass and every other unattended
//! caller of `docker_cli` behave exactly as before.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SIGNAL: AtomicI32 = AtomicI32::new(0);
/// The process group of the `docker` call in flight (0: none).
static ACTIVE_GROUP: AtomicI32 = AtomicI32::new(0);

/// The first signal is only recorded. A second one SIGKILLs the `docker`
/// group in flight, if any (it may ignore the forwarded signal, or not have
/// been sent it yet), then restores the default action and re-raises itself,
/// so the operator can always force-quit, leaving nothing running, even
/// during the forward grace or a step that does not poll [`pending`] (an
/// attached `shell`).
#[cfg(unix)]
extern "C" fn record(signal: libc::c_int) {
    if SIGNAL.swap(signal, Ordering::Relaxed) != 0 {
        let group = ACTIVE_GROUP.load(Ordering::Relaxed);
        // SAFETY: `kill`, `signal` and `raise` are async-signal-safe. `group`
        // leads a group `proc_exec` created for the docker child (never 0 or
        // an inherited group), so `kill(-group)` reaches only that child and
        // its descendants.
        unsafe {
            if group > 0 {
                libc::kill(-group, libc::SIGKILL);
            }
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }
}

/// Record the `docker` child's process group (its pid) while it runs, for
/// the second-signal kill. A no-op unless [`install`] has run, so the
/// daemon's concurrent docker calls never touch it.
pub fn set_active_group(pid: u32) {
    if INSTALLED.load(Ordering::Relaxed) {
        ACTIVE_GROUP.store(i32::try_from(pid).unwrap_or(0), Ordering::Relaxed);
    }
}

/// The `docker` call has returned (its group is gone or was killed).
pub fn clear_active_group() {
    ACTIVE_GROUP.store(0, Ordering::Relaxed);
}

/// Trap SIGINT and SIGTERM for this operator command.
///
/// # Errors
/// When a handler cannot be installed; the caller should warn and go on
/// (Ctrl-C then behaves as before this module).
pub fn install() -> anyhow::Result<()> {
    #[cfg(unix)]
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores to a lock-free atomic. Only the
        // short-lived operator CLI process calls this, never the daemon.
        if unsafe { libc::signal(signal, record as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    INSTALLED.store(true, Ordering::Relaxed);
    Ok(())
}

/// The signal an operator sent, once [`install`] has run; else `None`.
#[must_use]
pub fn pending() -> Option<i32> {
    if !INSTALLED.load(Ordering::Relaxed) {
        return None;
    }
    let signal = SIGNAL.load(Ordering::Relaxed);
    (signal != 0).then_some(signal)
}

/// A `docker` call stopped (or never started) because the operator sent
/// this signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted(pub i32);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "interrupted by signal {} (forwarded to the running `docker` command)",
            self.0
        )
    }
}

impl std::error::Error for Interrupted {}
