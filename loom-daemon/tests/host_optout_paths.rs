//! Issue #10179: the durable host opt-out refuses on every start /
//! re-provision path. Each test drives the real `loom-daemon` binary against a
//! temp HOME / marker, never the machine-level Loom dir.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn p(&self) -> &Path {
        self.dir.path()
    }
    fn desired(&self) -> PathBuf {
        self.p().join("autonomy-desired")
    }
    fn disabled(&self) -> PathBuf {
        self.p().join("autonomy-disabled")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        c.args(args)
            .current_dir(self.p())
            .env("HOME", self.p())
            .env("LOOM_AUTONOMY_MARKER", self.desired())
            .env("LOOM_SOCKET_PATH", self.p().join("loom-daemon.sock"))
            .env("LOOM_DAEMON_SYSTEMD", "0")
            .env("LOOM_DAEMON_LAUNCHD", "0")
            .env("USER", "alice")
            .env_remove("LOOM_MACHINE_CHECKOUT")
            .env_remove("LOOM_WORKSPACE")
            .env_remove("LOOM_DAEMON_SUPERVISOR");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
    /// A fake `loom-daemon-stop.sh` that records that it ran.
    fn fake_stop(&self) -> PathBuf {
        let s = self.p().join("fake-stop.sh");
        std::fs::write(&s, format!("#!/bin/sh\ntouch {}/stop-ran\n", self.p().display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        s
    }
    fn disable(&self) {
        let o = self
            .cmd(&["host", "disable", "--reason", "cost freeze"])
            .env("LOOM_HOST_STOP_SCRIPT", self.fake_stop())
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn assert_refused(o: &Output) {
    assert_eq!(o.status.code(), Some(1), "{}", text(o));
    let t = text(o);
    assert!(t.contains("cost freeze"), "reason: {t}");
    assert!(t.contains("alice"), "who: {t}");
    assert!(t.contains("when:"), "when: {t}");
    assert!(t.contains("loom-daemon host enable"), "enable hint: {t}");
}

#[test]
fn disable_writes_marker_stops_daemon_and_clears_desired() {
    let h = Host::new();
    std::fs::write(h.desired(), "started_at=x\n").unwrap();
    h.disable();
    assert!(h.disabled().exists());
    assert!(!h.desired().exists());
    assert!(h.p().join("stop-ran").exists(), "stop script ran");
    let s = h.run(&["host", "status"]);
    assert!(text(&s).contains("disabled by operator: cost freeze ("), "{}", text(&s));
    // Idempotent.
    h.disable();
}

#[test]
fn disable_requires_reason() {
    let h = Host::new();
    assert!(!h.run(&["host", "disable"]).status.success());
    let o = h.run(&["host", "disable", "--reason", "  "]);
    assert!(!o.status.success());
    assert!(!h.disabled().exists());
}

#[test]
fn enable_clears_marker_idempotently_and_starts_nothing() {
    let h = Host::new();
    h.disable();
    let _ = std::fs::remove_file(h.p().join("stop-ran"));
    for _ in 0..2 {
        let o = h.run(&["host", "enable"]);
        assert!(o.status.success(), "{}", text(&o));
        assert!(text(&o).contains("loom-daemon-start.sh"));
    }
    assert!(!h.disabled().exists());
    assert!(!h.desired().exists(), "enable must not arm autonomy");
    assert_eq!(h.run(&["host", "check"]).status.code(), Some(0));
}

#[test]
fn daemon_start_refuses_with_no_side_effects() {
    let h = Host::new();
    h.disable();
    assert_refused(&h.run(&["daemon-start"]));
    assert!(!h.desired().exists());
    assert!(!h.p().join(".loom").exists());
}

#[test]
fn daemon_start_heal_watchdog_only_also_refuses() {
    let h = Host::new();
    h.disable();
    assert_refused(&h.run(&["daemon-start", "--heal-watchdog-only"]));
}

#[test]
fn watchdog_tick_refuses_and_does_not_recover() {
    let h = Host::new();
    h.disable();
    // Even a stale desired marker must not make the watchdog revive anything.
    std::fs::write(h.desired(), "started_at=x\n").unwrap();
    assert_refused(&h.run(&["daemon-watchdog"]));
}

#[test]
fn daemon_update_refuses_before_any_rebuild_or_restart() {
    let h = Host::new();
    h.disable();
    assert_refused(&h.run(&["daemon-update", "--no-restart"]));
    assert_refused(&h.run(&["daemon-update"]));
}

#[test]
fn daemon_startup_itself_refuses() {
    let h = Host::new();
    h.disable();
    let o = h
        .cmd(&[])
        .env("LOOM_DAEMON_SUPERVISOR", "systemd")
        .output()
        .unwrap();
    assert_refused(&o);
}

#[test]
fn status_and_health_report_disabled_and_exit_zero() {
    let h = Host::new();
    h.disable();
    for sub in ["status", "health"] {
        let o = h.run(&[sub]);
        assert_eq!(o.status.code(), Some(0), "{sub}: {}", text(&o));
        assert!(text(&o).contains("disabled by operator: cost freeze ("), "{sub}: {}", text(&o));
    }
}

#[test]
fn malformed_marker_fails_closed_with_generic_message() {
    let h = Host::new();
    std::fs::write(h.disabled(), "garbage\n").unwrap();
    let o = h.run(&["daemon-start"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("failing closed"), "{}", text(&o));
    assert!(text(&o).contains("loom-daemon host enable"));
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Shell entry points call `loom-daemon host check` via PATH.
fn run_shell(h: &Host, script: &Path, args: &[&str]) -> Output {
    let bin_dir = h.p().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let link = bin_dir.join("loom-daemon");
    if !link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_loom-daemon"), &link).unwrap();
    }
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    Command::new("bash")
        .arg(script)
        .args(args)
        .current_dir(h.p())
        .env("PATH", path)
        .env("HOME", h.p())
        .env("LOOM_AUTONOMY_MARKER", h.desired())
        .env("LOOM_SOCKET_PATH", h.p().join("loom-daemon.sock"))
        .env("USER", "alice")
        .output()
        .unwrap()
}

#[test]
fn resync_installed_refuses() {
    let h = Host::new();
    h.disable();
    let o =
        run_shell(&h, &repo_root().join("defaults/scripts/resync-installed.sh"), &["--dry-run"]);
    assert_refused(&o);
}

#[test]
fn installer_refuses() {
    let h = Host::new();
    h.disable();
    let target = h.p().join("target-repo");
    std::fs::create_dir_all(&target).unwrap();
    let o =
        run_shell(&h, &repo_root().join("scripts/install-loom.sh"), &[target.to_str().unwrap()]);
    assert_refused(&o);
    assert!(!target.join(".loom").exists(), "installer must have no side effects");
}
