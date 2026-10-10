// #7855 end to end through the BUILT binary: `loom-daemon daemon-watchdog`
// ticks against a tempdir host whose supervisor, `ps`, `uptime` and probe
// binary are all recording stubs on PATH.
//
// The Rust unit tests in `src/watchdog/hang_recover_tests.rs` pin the decision
// and the durable state with injected closures. What they cannot show is the
// WIRING: that `ipc_probe`'s CONFIRMED branch actually reaches the decision,
// that the marker field the start writes is what the scheduled tick reads, and
// that the command the real tick issues is the supervised restart — and only
// that. This drives exactly that path. Nothing here restarts a real daemon:
// `systemctl` is a script that appends its argv to a log, the "daemon" is a
// `sleep` child this test owns, and every path is inside a tempdir (the socket
// path pins `<loom_dir>`, so host opt-out and every state file resolve there,
// never to `~/.loom`).
//
// systemd, not launchd, for the same reason as
// `integration_drain_exit_then_watchdog_recovers.rs`: the launchd tier is
// disabled off Darwin, and this runs on Linux CI. The launchd argv is pinned by
// the unit tests.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![cfg(unix)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

struct Host {
    _dir: tempfile::TempDir,
    root: PathBuf,
    stubs: PathBuf,
    marker: PathBuf,
    log: PathBuf,
    systemctl_log: PathBuf,
    daemon: Child,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

const UNIT: &str = "loom-daemon-hang-recover-test.service";

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/usr/bin/env bash\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A host whose daemon is ALIVE under a stubbed systemd unit, 2h old (past
/// every startup grace), whose IPC probe always fails, and whose heartbeat is
/// `heartbeat_age` seconds old. `hang_field` is the marker's
/// `watchdog_hang_recover=` value (`None` = the line is absent).
fn host(hang_field: Option<&str>, heartbeat_age: u64) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let stubs = root.join("stubs");
    std::fs::create_dir_all(&stubs).unwrap();
    let systemctl_log = root.join("systemctl.log");
    std::fs::write(&systemctl_log, "").unwrap();

    let daemon = Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = daemon.id();

    script(
        &stubs.join("systemctl"),
        &format!(
            "echo \"$*\" >> '{log}'\n\
             case \"$*\" in\n\
               *'-p MainPID'*) echo {pid} ;;\n\
               *'-p LoadState'*) echo loaded ;;\n\
               *'-p ExecMainCode'*) echo '' ;;\n\
               *'-p ExecMainStatus'*) echo '' ;;\n\
             esac\n\
             exit 0\n",
            log = systemctl_log.display()
        ),
    );
    script(&stubs.join("ps"), "echo 02:00:00\n");
    script(
        &stubs.join("uptime"),
        "echo '10:00:00 up 1 day, load average: 16.80, 15.20, 14.00'\n",
    );
    script(
        &stubs.join("loom-daemon-mock"),
        "echo 'Could not reach loom-daemon at /tmp/x.sock: round-trip timed out after 5s' >&2\n\
         exit 1\n",
    );

    let heartbeat = root.join("daemon.heartbeat");
    std::fs::write(&heartbeat, format!("0 pid={pid}\n")).unwrap();
    let mtime = SystemTime::now() - Duration::from_secs(heartbeat_age);
    std::fs::File::options()
        .write(true)
        .open(&heartbeat)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    let marker = root.join("autonomy-desired");
    let mut body = format!(
        "started_at=2026-10-08T00:00:00Z\n\
         heartbeat_file={hb}\n\
         heartbeat_interval_secs=60\n\
         use_launchd=false\n\
         use_systemd=true\n\
         systemd_unit={UNIT}\n\
         socket_path={sock}\n",
        hb = heartbeat.display(),
        sock = root.join("loom-daemon.sock").display(),
    );
    if let Some(v) = hang_field {
        body.push_str(&format!("watchdog_hang_recover={v}\n"));
    }
    std::fs::write(&marker, body).unwrap();

    Host {
        log: root.join("watchdog.log"),
        _dir: dir,
        root,
        stubs,
        marker,
        systemctl_log,
        daemon,
    }
}

/// One scheduled-job tick. The job's environment carries only paths, so no
/// `LOOM_WATCHDOG_HANG_RECOVER*` is set here: the marker is the only channel.
fn tick(h: &Host) -> i32 {
    let path = format!("{}:{}", h.stubs.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.arg("daemon-watchdog");
    for k in [
        "LOOM_WATCHDOG_HANG_RECOVER",
        "LOOM_WATCHDOG_HANG_RECOVER_CONFIRMATIONS",
        "LOOM_WATCHDOG_HANG_RECOVER_COOLDOWN_SECS",
        "LOOM_WATCHDOG_HANG_RECOVER_MAX_UNHEALED",
        "LOOM_WATCHDOG_HANG_RECOVER_STATE",
        "LOOM_WATCHDOG_HANG_RECOVER_STREAK_STATE",
        "LOOM_DAEMON_HEARTBEAT_STALE_SECS",
        "LOOM_LAUNCHD_DOMAIN",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("PATH", path)
        .env("LOOM_SOCKET_PATH", h.root.join("loom-daemon.sock"))
        .env("LOOM_AUTONOMY_MARKER", &h.marker)
        .env("LOOM_WATCHDOG_LOG", &h.log)
        .env("LOOM_DAEMON_LAUNCHD", "0")
        .env("LOOM_PID_FILE", "")
        .env("LOOM_WORKSPACE", "")
        .env("LOOM_MACHINE_CHECKOUT", "")
        .env("LOOM_WATCHDOG_IPC_PROBE", "1")
        .env("LOOM_DAEMON_BIN", h.stubs.join("loom-daemon-mock"))
        .env("LOOM_WATCHDOG_STATUS_PROBE_TIMEOUT_SECS", "5")
        // Every probe failure is CONFIRMED, so the dual-signal streak — not the
        // raw IPC threshold — is what this test counts.
        .env("LOOM_WATCHDOG_IPC_PROBE_FAIL_THRESHOLD", "1")
        .env("LOOM_WATCHDOG_AUTO_RECOVER", "0")
        .env("LOOM_WATCHDOG_ESCALATE", "0")
        .env("LOOM_WATCHDOG_RECOVERY_STATE", h.root.join(".watchdog-recovery-state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.status().unwrap().code().unwrap_or(-1)
}

fn restarts(h: &Host) -> Vec<String> {
    std::fs::read_to_string(&h.systemctl_log)
        .unwrap()
        .lines()
        .filter(|l| !l.contains(" show "))
        .map(str::to_string)
        .collect()
}

fn log(h: &Host) -> String {
    std::fs::read_to_string(&h.log).unwrap_or_default()
}

#[test]
fn opted_in_host_restarts_once_via_systemctl_after_three_dual_signal_ticks() {
    let h = host(Some("true"), 1500);

    for n in 1..=2 {
        assert_eq!(tick(&h), 1, "tick {n} reports the CONFIRMED hang");
        assert!(restarts(&h).is_empty(), "tick {n} must not restart: {:?}", restarts(&h));
    }
    let text = log(&h);
    assert!(text.contains("IPC UNRESPONSIVE (CONFIRMED)"), "{text}");
    assert!(text.contains("tick 2 of 3"), "{text}");

    assert_eq!(tick(&h), 1);
    assert_eq!(
        restarts(&h),
        vec![format!("--user restart {UNIT}")],
        "exactly one supervised restart, and nothing else sent to the supervisor"
    );
    let text = log(&h);
    for want in [
        "[DIVERGENCE] HANG RECOVERY (#7855, opt-in via marker)",
        &format!("'systemctl --user restart {UNIT}'"),
        &format!("wedged pid {}", h.daemon.id()),
        "supervisor command exited 0",
        "failed the IPC round-trip",
        "> 300s threshold",
        "for 3 consecutive CONFIRMED ticks",
        "host load average 16.80, 15.20, 14.00",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(h.root.join(".watchdog-hang-recover-state").exists());

    // Still wedged (the stub did not really restart anything): the durable
    // cooldown refuses a second restart, and the report keeps its commands.
    for _ in 0..4 {
        assert_eq!(tick(&h), 1);
    }
    assert_eq!(restarts(&h).len(), 1, "one restart per cooldown window: {:?}", restarts(&h));
    let text = log(&h);
    assert!(text.contains("at most one is issued per 1800s"), "{text}");
    assert!(text.contains("loom-daemon-stop.sh"), "{text}");
}

#[test]
fn default_host_without_the_marker_field_stays_report_only() {
    let h = host(None, 1500);
    for _ in 0..4 {
        assert_eq!(tick(&h), 1);
    }
    assert!(restarts(&h).is_empty(), "{:?}", restarts(&h));
    let text = log(&h);
    assert!(text.contains("IPC UNRESPONSIVE (CONFIRMED)"), "{text}");
    assert!(text.contains("No automatic kill/restart is attempted"), "{text}");
    assert!(text.contains("REPORT-ONLY"), "{text}");
    assert!(!text.contains("HANG RECOVERY"), "{text}");
    assert!(!h.root.join(".watchdog-hang-recover-state").exists());
    assert!(!h.root.join(".watchdog-hang-streak").exists());
}

#[test]
fn opted_in_host_with_a_fresh_heartbeat_never_restarts() {
    // Failed IPC alone is what heavy legitimate load looks like.
    let h = host(Some("true"), 10);
    for _ in 0..4 {
        assert_eq!(tick(&h), 1);
    }
    assert!(restarts(&h).is_empty(), "{:?}", restarts(&h));
    let text = log(&h);
    assert!(text.contains("the heartbeat is FRESH"), "{text}");
}
