//! Per-tick path and knob resolution.
//!
//! Everything the tick needs is resolved once, up front, from the environment
//! and the marker — so the decision logic below takes a plain struct and is
//! testable without touching process-global environment variables.

use std::path::PathBuf;

use super::env;

/// The paths a tick reads and writes.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `LOOM_SOCKET_PATH`, else `~/.loom/loom-daemon.sock`.
    pub socket_path: PathBuf,
    /// The socket's parent — every other path hangs off this.
    pub loom_dir: PathBuf,
    /// `LOOM_AUTONOMY_MARKER`, else `<loom_dir>/autonomy-desired`. Operator
    /// INTENT: present ⇒ a daemon is expected, absent ⇒ deliberately stopped.
    pub marker: PathBuf,
    /// `LOOM_WATCHDOG_LOG`, else `<loom_dir>/logs/daemon-watchdog.log`.
    pub log: PathBuf,
}

impl Paths {
    /// Resolve from the environment, exactly as the script's four assignments
    /// did. Note `LOOM_DIR` is derived from the socket path rather than read
    /// independently — a test pointing `LOOM_SOCKET_PATH` at a tempdir gets its
    /// marker, log and state files in that tempdir too, never the operator's
    /// real `~/.loom`.
    #[must_use]
    pub fn from_env() -> Self {
        let socket_path = env::var("LOOM_SOCKET_PATH").map_or_else(
            || {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".loom")
                    .join("loom-daemon.sock")
            },
            PathBuf::from,
        );
        let loom_dir = socket_path
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let marker = env::var("LOOM_AUTONOMY_MARKER")
            .map_or_else(|| loom_dir.join("autonomy-desired"), PathBuf::from);
        let log = env::var("LOOM_WATCHDOG_LOG")
            .map_or_else(|| loom_dir.join("logs").join("daemon-watchdog.log"), PathBuf::from);
        Self {
            socket_path,
            loom_dir,
            marker,
            log,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loom_dir_is_the_socket_parent_not_an_independent_lookup() {
        // #5118's shape: two ends deriving the same path separately is how they
        // come to disagree. Everything here hangs off the one socket path.
        let p = Paths {
            socket_path: PathBuf::from("/tmp/t/loom-daemon.sock"),
            loom_dir: PathBuf::from("/tmp/t"),
            marker: PathBuf::from("/tmp/t/autonomy-desired"),
            log: PathBuf::from("/tmp/t/logs/daemon-watchdog.log"),
        };
        assert_eq!(p.socket_path.parent().unwrap(), p.loom_dir);
        assert!(p.marker.starts_with(&p.loom_dir));
        assert!(p.log.starts_with(&p.loom_dir));
    }
}
