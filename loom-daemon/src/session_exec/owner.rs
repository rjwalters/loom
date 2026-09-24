//! Stable launcher lifetime, never a numeric-PID liveness/kill lookup after
//! registration. A daemon dying while its adapter shell survives must still
//! revoke that invocation's heartbeat; PID reuse cannot renew ownership.
use anyhow::{bail, Result};

pub(super) struct Owner(i32);

impl Owner {
    pub(super) fn new(pid: i32) -> Result<Self> {
        if pid <= 1 {
            bail!("session launcher is already orphaned");
        }
        #[cfg(target_os = "linux")]
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        #[cfg(target_os = "macos")]
        let fd = unsafe {
            let fd = libc::kqueue();
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let event = libc::kevent {
                ident: pid as _,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            if libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) < 0 {
                libc::close(fd);
                return Err(std::io::Error::last_os_error().into());
            }
            fd
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let fd = -1;
        if fd < 0 {
            bail!("cannot watch session launcher lifetime: {}", std::io::Error::last_os_error());
        }
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(Self(fd))
    }

    pub(super) fn alive(&self) -> bool {
        #[cfg(target_os = "linux")]
        unsafe {
            let mut event = libc::pollfd {
                fd: self.0,
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut event, 1, 0) == 0
        }
        #[cfg(target_os = "macos")]
        unsafe {
            let mut event = std::mem::zeroed();
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            libc::kevent(self.0, std::ptr::null(), 0, &mut event, 1, &timeout) == 0
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        false
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}
