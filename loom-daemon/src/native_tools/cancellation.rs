//! Tool subprocess cancellation only; does not change daemon signal handling.
use std::sync::atomic::{AtomicBool, Ordering};
static CANCELLED: AtomicBool = AtomicBool::new(false);
pub fn requested() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}
#[cfg(unix)]
extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}
pub fn install() -> anyhow::Result<()> {
    #[cfg(unix)]
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to a lock-free atomic; this command
        // owns its process and installs handlers before spawning any tools.
        if unsafe { libc::signal(signal, cancel as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
