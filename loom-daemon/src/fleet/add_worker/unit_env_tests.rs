//! The fleet worker's daemon unit carries only
//! supervisor-structural env, and no later provisioning step replaces it.
//!
//! The defect was not in `render_daemon_unit` (a static template) but in the
//! `daemon-watchdog` step that followed it: a bare `loom-daemon-start.sh`
//! missed its "already running" guard and re-rendered the unit from its own
//! process env, baking `LOOM_WORK_FINDER=0` and friends into it. These tests
//! run both rendered scripts against a stubbed `$HOME` with a polluted
//! environment and assert on the unit file they leave behind.
//!
//! A sibling of `tests.rs`, which is over the file-size ratchet's threshold
//! (`.loom/docs/file-size-policy.md`).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use super::*;
use crate::fleet::PlanEntry;

/// The env a polluted invoking shell (or a start script's own resolved
/// defaults) would carry. None of it may reach the unit.
const POLLUTED: &[(&str, &str)] = &[
    ("LOOM_WORK_FINDER", "0"),
    ("LOOM_MAIN_HEALTH_GATE", "0"),
    ("LOOM_FORCE_SCOPE", "protected"),
    ("LOOM_GUARD_DECISION_LOG", "1"),
    ("LOOM_ROLE_RUNNER", "codex"),
    ("LOOM_FOO", "x"),
];

/// The only `Environment=` keys a freshly provisioned fleet unit may carry.
const STRUCTURAL: &[&str] = &["PATH", "LOOM_DAEMON_SUPERVISOR"];

fn write_exec(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake worker `$HOME`: `systemctl`/`loginctl` stubs on the canonical PATH
/// (`~/.local/bin` comes first), an enabled-timer switch, and a
/// `loom-daemon-start.sh` stub that records its argv and, when run WITHOUT
/// `--heal-watchdog-only`, does what the real full start did to the unit.
fn fake_home(timer_enabled: bool) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let enabled = if timer_enabled { "exit 0" } else { "exit 1" };
    write_exec(
        &h.join(".local/bin/systemctl"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *is-enabled*loom-daemon-watchdog.timer*) {enabled};; esac\nexit 0\n"
        ),
    );
    write_exec(&h.join(".local/bin/loginctl"), "#!/bin/sh\nexit 0\n");
    write_exec(
        &h.join(".local/share/loom/defaults/scripts/cli/loom-daemon-start.sh"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/start-argv\"\n\
         [ \"$1\" = --heal-watchdog-only ] && exit 0\n\
         env | grep '^LOOM_' | sed 's/^/Environment=/' \
         >> \"$HOME/.config/systemd/user/loom-daemon.service\"\n",
    );
    std::fs::create_dir_all(h.join("loom-workspaces/loom")).unwrap();
    std::fs::create_dir_all(h.join(".loom")).unwrap();
    std::fs::write(h.join(".loom/autonomy-desired"), "").unwrap();
    home
}

/// Run a rendered step script under `home` with [`POLLUTED`] exported.
fn run_polluted(script: &str, home: &Path) -> std::process::Output {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(script).env("HOME", home);
    for (k, v) in POLLUTED {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn unit_env_keys(home: &Path) -> Vec<String> {
    let unit =
        std::fs::read_to_string(home.join(".config/systemd/user/loom-daemon.service")).unwrap();
    crate::daemon_start::render::systemd_env_keys(&unit)
}

#[test]
fn a_polluted_env_yields_a_unit_with_only_structural_env() {
    let home = fake_home(true);
    let out = run_polluted(&render_daemon_unit("loom-workspaces/loom"), home.path());
    assert!(
        out.status.success(),
        "daemon-unit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(unit_env_keys(home.path()), STRUCTURAL);

    let out = run_polluted(&render_daemon_watchdog("loom-workspaces/loom"), home.path());
    assert!(
        out.status.success(),
        "daemon-watchdog failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        unit_env_keys(home.path()),
        STRUCTURAL,
        "daemon-watchdog must not re-render the fleet unit (2am#4484)"
    );
    let argv = std::fs::read_to_string(home.path().join("start-argv")).unwrap();
    assert_eq!(argv, "--heal-watchdog-only\n");
}

#[test]
fn daemon_watchdog_fails_when_the_timer_is_still_missing() {
    let home = fake_home(false);
    let out = run_polluted(&render_daemon_watchdog("loom-workspaces/loom"), home.path());
    assert!(!out.status.success(), "a no-op heal must not report success");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not provisioned"));
}

#[test]
fn no_plan_step_runs_a_full_daemon_start() {
    let secrets = Secrets {
        pat: Some("pat".to_string()),
        accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
    };
    let plan = build_plan(&tests::base_config(), &secrets);
    for entry in &plan.entries {
        let PlanEntry::Step(step) = entry else {
            continue;
        };
        for line in step
            .apply
            .lines()
            .filter(|l| l.contains("loom-daemon-start.sh"))
        {
            assert!(
                line.contains("--heal-watchdog-only") || line.trim_start().starts_with('#'),
                "step {} runs a full start, which re-renders the unit: {line}",
                step.name
            );
        }
    }
}
