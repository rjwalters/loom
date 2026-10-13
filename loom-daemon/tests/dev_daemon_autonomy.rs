//! Issue #9703: `scripts/dev-daemon.sh` starts the daemon with autonomy off
//! unless `--autonomy` is passed. A daemon in a clone of rjwalters/loom acts on
//! rjwalters/loom, so the developer default must not run the work finder, the
//! role runner, claim reconciliation or auto-update.
//!
//! The script runs for real against stub `scripts/` siblings: the stub
//! `start-daemon.sh` records the environment the daemon would inherit and
//! writes no pid file, so the script stops at "Failed to start daemon"
//! instead of tailing a log forever.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

const KILL_SWITCHES: [&str; 4] = [
    "LOOM_WORK_FINDER",
    "LOOM_ROLE_RUNNER",
    "LOOM_STALE_CLAIM_RECONCILE",
    "LOOM_AUTO_UPDATE",
];

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the real script with `args`; return the daemon's would-be environment
/// and the script's stdout.
fn run(args: &[&str]) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let scripts = dir.path().join("scripts");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/dev-daemon.sh");
    std::fs::copy(&real, scripts.join("dev-daemon.sh")).unwrap();
    let env_file = dir.path().join("daemon-env");
    write_exec(
        &scripts.join("start-daemon.sh"),
        &format!("#!/bin/sh\nenv > '{}'\n", env_file.display()),
    );
    write_exec(&scripts.join("clean-tmux.sh"), "#!/bin/sh\nexit 0\n");
    write_exec(&scripts.join("stop-daemon.sh"), "#!/bin/sh\nexit 0\n");
    write_exec(&bin.join("clear"), "#!/bin/sh\nexit 0\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/dev-daemon.sh")
        .args(args)
        .current_dir(dir.path())
        .env("PATH", path)
        .env("HOME", dir.path());
    for k in KILL_SWITCHES {
        cmd.env_remove(k);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(1), "stops at the missing pid file: {stdout}");
    assert!(stdout.contains("Failed to start daemon"), "{stdout}");
    (std::fs::read_to_string(&env_file).expect("start-daemon.sh ran"), stdout)
}

fn set_to(env: &str, key: &str) -> Option<String> {
    env.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
}

#[test]
fn autonomy_is_off_by_default() {
    let (env, stdout) = run(&[]);
    for k in KILL_SWITCHES {
        assert_eq!(set_to(&env, k).as_deref(), Some("0"), "{k} in:\n{env}");
    }
    assert!(
        stdout.contains("autonomy OFF (dev default; rerun with --autonomy to enable)"),
        "{stdout}"
    );
}

#[test]
fn autonomy_flag_leaves_the_daemon_config_in_charge() {
    let (env, stdout) = run(&["--autonomy"]);
    for k in KILL_SWITCHES {
        assert_eq!(set_to(&env, k), None, "{k} must be unset with --autonomy:\n{env}");
    }
    assert!(stdout.contains("autonomy ON (--autonomy given)"), "{stdout}");
}
