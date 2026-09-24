//! The autonomy-desired intent marker (#4011).
//!
//! Its LIFETIME is operator INTENT, not process liveness: only an
//! operator-initiated `loom-daemon-stop.sh` removes it, and it is deliberately
//! preserved across the internal stop `loom-daemon-update.sh` performs. The
//! watchdog reads it to decide whether a missing daemon is a silent failure
//! (marker present ⇒ report) or a deliberate stop (marker absent ⇒ stay quiet).

use std::path::Path;

/// Everything `write_intent_marker` records, so the watchdog can probe reality
/// without re-deriving any of it.
pub struct IntentMarker<'a> {
    pub repo_root: &'a Path,
    pub pid_file: &'a Path,
    pub heartbeat_file: &'a Path,
    pub heartbeat_interval_secs: &'a str,
    pub use_launchd: bool,
    pub launchd_label: &'a str,
    pub use_systemd: bool,
    pub systemd_unit: &'a str,
    pub socket_path: &'a Path,
}

/// `write_intent_marker <use_launchd> <label> [use_systemd] [unit]`.
///
/// `work_finder` / `health_gate` (#5437) persist THIS invocation's actual
/// resolved values. On the nohup fallback tier — which never renders a
/// plist/unit — they are the only durable record of "was the daemon most
/// recently started autonomously?", and without them every bare restart
/// following any prior start looked like a downgrade.
///
/// Written under `umask 077`: the marker records paths and a label, and the
/// shell chose 0600 deliberately.
pub fn write(loom_dir: &Path, marker_path: &Path, m: &IntentMarker) {
    let _ = std::fs::create_dir_all(loom_dir);
    let started_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let work_finder = std::env::var("LOOM_WORK_FINDER").unwrap_or_default();
    let health_gate = std::env::var("LOOM_MAIN_HEALTH_GATE").unwrap_or_default();

    let body = format!(
        "# loom autonomy-desired marker (issue #4011)\n\
         # Presence ⇒ a loom-daemon is EXPECTED to be running on this host. Written by\n\
         # loom-daemon-start.sh on a successful start; removed ONLY by an\n\
         # operator-initiated loom-daemon-stop.sh (preserved across update.sh restarts).\n\
         # Do not hand-edit — delete via loom-daemon-stop.sh so the watchdog stays quiet.\n\
         started_at={started_at}\n\
         repo_root={repo_root}\n\
         pid_file={pid_file}\n\
         heartbeat_file={heartbeat_file}\n\
         heartbeat_interval_secs={heartbeat_interval_secs}\n\
         use_launchd={use_launchd}\n\
         launchd_label={launchd_label}\n\
         use_systemd={use_systemd}\n\
         systemd_unit={systemd_unit}\n\
         socket_path={socket_path}\n\
         work_finder={work_finder}\n\
         health_gate={health_gate}\n",
        repo_root = m.repo_root.display(),
        pid_file = m.pid_file.display(),
        heartbeat_file = m.heartbeat_file.display(),
        heartbeat_interval_secs = m.heartbeat_interval_secs,
        use_launchd = m.use_launchd,
        launchd_label = m.launchd_label,
        use_systemd = m.use_systemd,
        systemd_unit = m.systemd_unit,
        socket_path = m.socket_path.display(),
    );
    write_private(marker_path, &body);
}

/// `( umask 077; cat > file )` — create at 0600 rather than the process umask.
fn write_private(path: &Path, body: &str) {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            let _ = f.write_all(body.as_bytes());
            return;
        }
    }
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_records_the_resolved_autonomy_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        write(
            dir.path(),
            &marker,
            &IntentMarker {
                repo_root: Path::new("/repo"),
                pid_file: Path::new("/repo/.loom/.daemon.pid"),
                heartbeat_file: Path::new("/h/daemon.heartbeat"),
                heartbeat_interval_secs: "60",
                use_launchd: false,
                launchd_label: "",
                use_systemd: true,
                systemd_unit: "loom-daemon.service",
                socket_path: Path::new("/h/loom-daemon.sock"),
            },
        );
        let text = std::fs::read_to_string(&marker).expect("read");
        assert!(text.contains("use_launchd=false\n"));
        assert!(text.contains("use_systemd=true\n"));
        assert!(text.contains("systemd_unit=loom-daemon.service\n"));
        assert!(text.contains("heartbeat_interval_secs=60\n"));
        // #5437's two fields must always be PRESENT, even when empty, because
        // the downgrade check distinguishes "no field" from "field says 0".
        assert!(text.contains("\nwork_finder="));
        assert!(text.contains("\nhealth_gate="));
    }

    #[cfg(unix)]
    #[test]
    fn the_marker_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        write(
            dir.path(),
            &marker,
            &IntentMarker {
                repo_root: Path::new("/r"),
                pid_file: Path::new("/p"),
                heartbeat_file: Path::new("/h"),
                heartbeat_interval_secs: "60",
                use_launchd: true,
                launchd_label: "l",
                use_systemd: false,
                systemd_unit: "",
                socket_path: Path::new("/s"),
            },
        );
        let mode = std::fs::metadata(&marker)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "umask 077 in the shell");
    }
}
