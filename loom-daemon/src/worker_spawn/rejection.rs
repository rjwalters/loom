//! Per-spawn preflight results. The launcher can report through its own
//! connected socket, which is closed on exec before any harness can run.
use super::{stage_reason, LaunchStage, PREFLIGHT_REJECTED};
use std::process::Command;

const REPORT_FD: &str = "LOOM_LAUNCH_REJECTION_FD";
const UNTYPED: &str = "rejected";

#[cfg(unix)]
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{net::UnixDatagram, process::CommandExt},
    },
};

#[derive(Debug)]
pub(crate) struct Receiver(#[cfg(unix)] UnixDatagram);

impl Receiver {
    pub(crate) fn prepare(command: &mut Command, enabled: bool) -> Option<Self> {
        command.env_remove(REPORT_FD);
        #[cfg(unix)]
        if enabled {
            match Self::pair(command) {
                Ok(receiver) => return Some(receiver),
                Err(error) => log::warn!("cannot observe worker launch rejection: {error}"),
            }
        }
        #[cfg(not(unix))]
        let _ = enabled;
        None
    }

    #[cfg(unix)]
    fn pair(command: &mut Command) -> io::Result<Self> {
        let (receiver, sender) = UnixDatagram::pair()?;
        receiver.set_nonblocking(true)?;
        sender.set_nonblocking(true)?;
        command.env(REPORT_FD, sender.as_raw_fd().to_string());
        // SAFETY: the closure owns the socket and only uses async-signal-safe
        // fcntl after fork. The parent's copy stays close-on-exec.
        unsafe {
            command.pre_exec(move || close_on_exec(sender.as_raw_fd(), false));
        }
        Ok(Self(receiver))
    }

    pub(crate) fn reason(self) -> Option<&'static str> {
        #[cfg(unix)]
        {
            let mut bytes = [0; 32];
            let len = self.0.recv(&mut bytes).ok()?;
            let token = std::str::from_utf8(&bytes[..len]).ok()?;
            reason_for_token(token)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

fn reason_for_token(token: &str) -> Option<&'static str> {
    if token == UNTYPED {
        Some(PREFLIGHT_REJECTED)
    } else {
        LaunchStage::parse(token).map(stage_reason)
    }
}

pub(super) struct Reporter(#[cfg(unix)] UnixDatagram);

impl Reporter {
    pub(super) fn from_environment() -> Option<Self> {
        #[cfg(unix)]
        {
            let value = std::env::var(REPORT_FD).ok()?;
            let fd = value.parse::<RawFd>().ok().filter(|fd| *fd > 2)?;
            let mut kind: libc::c_int = 0;
            let mut len = std::mem::size_of_val(&kind) as libc::socklen_t;
            // SAFETY: `kind` and `len` are valid writable buffers for this
            // option. Check the inherited descriptor before taking ownership.
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    std::ptr::addr_of_mut!(kind).cast(),
                    &mut len,
                )
            };
            if rc != 0 || kind != libc::SOCK_DGRAM {
                return None;
            }
            close_on_exec(fd, true).ok()?;
            // Duplicate first so ownership never depends on a descriptor
            // number from the environment. SAFETY: fcntl returns a distinct
            // owned descriptor, or an error.
            let owned = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if owned < 0 {
                return None;
            }
            Some(Self(unsafe { UnixDatagram::from_raw_fd(owned) }))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    pub(super) fn report(self, stage: Option<LaunchStage>) {
        #[cfg(not(unix))]
        let _ = (self, stage);
        #[cfg(unix)]
        {
            let token = stage.map_or(UNTYPED, LaunchStage::as_str);
            let _ = self.0.send(token.as_bytes());
        }
    }

    pub(super) fn scrub(command: &mut Command) {
        command.env_remove(REPORT_FD);
    }
}

#[cfg(unix)]
fn close_on_exec(fd: RawFd, enabled: bool) -> io::Result<()> {
    let flag = if enabled { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: this changes descriptor flags and touches no process-global
    // state or Rust memory. Both values are valid for F_SETFD.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flag) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rejection_results_use_only_closed_tokens() {
        assert_eq!(reason_for_token("credential"), Some("preflight_credential"));
        assert_eq!(reason_for_token("rejected"), Some(PREFLIGHT_REJECTED));
        assert_eq!(reason_for_token(""), None);
        assert_eq!(reason_for_token("credential secret"), None);
    }

    #[test]
    #[cfg(unix)]
    fn only_this_attempt_receives_its_report() {
        let (old, old_sender) = UnixDatagram::pair().unwrap();
        old.set_nonblocking(true).unwrap();
        drop(old_sender);
        let (current, current_sender) = UnixDatagram::pair().unwrap();
        current.set_nonblocking(true).unwrap();
        Reporter(current_sender).report(Some(LaunchStage::Credential));
        assert_eq!(Receiver(old).reason(), None);
        assert_eq!(Receiver(current).reason(), Some("preflight_credential"));
    }
}
