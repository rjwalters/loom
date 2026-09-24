//! Arming the autonomy-loss watchdog (#4011, #4260 sub-issue D) and healing a
//! provisioning GAP on an already-running daemon (#5343).
//!
//! The watchdog is the payload of a SECOND, SEPARATE scheduled job: a launchd
//! `StartInterval` job on Darwin, a `.timer` + `.service` pair under
//! `systemd --user` on Linux. Both share the property that matters — the
//! watchdog job owns **no long-lived process**, so it structurally cannot
//! crash and stay dead. That is the who-watches-the-watchdog answer.
//!
//! Everything here is **best-effort and non-fatal**. A watchdog that fails to
//! install must never fail the daemon start: a daemon running without a
//! watchdog is strictly better than no daemon at all.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::out;
use super::platform;
use super::render;

/// The paths and identity a provisioning pass needs.
pub struct Ctx {
    pub repo_root: PathBuf,
    pub loom_dir: PathBuf,
    pub intent_marker: PathBuf,
    pub socket_path: PathBuf,
    pub pid_file: PathBuf,
    pub plist_path_value: String,
    pub home: String,
}

/// `locate_watchdog_script()` — the installed copy first, then the `defaults/`
/// copy for a Loom source checkout that has not yet resynced.
#[must_use]
pub fn locate_watchdog_script(repo_root: &Path) -> Option<PathBuf> {
    [
        repo_root.join(".loom/scripts/cli/loom-daemon-watchdog.sh"),
        repo_root.join("defaults/scripts/cli/loom-daemon-watchdog.sh"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn missing_script_warning() {
    out::warn("watchdog: loom-daemon-watchdog.sh not found — skipping (autonomy-loss detection disabled).");
}

/// `provision_watchdog_job_launchd()`.
///
/// The unchanged-and-already-loaded skip is the #4862 double-fire fix:
/// `RunAtLoad=true` means every bootout+bootstrap cycle fires an extra
/// immediate run on top of the `StartInterval` cadence, and this function runs
/// on EVERY start, restart and self-update relaunch.
pub fn provision_launchd(ctx: &Ctx) {
    if !platform::have("launchctl") {
        out::warn("watchdog: launchctl not found — skipping.");
        return;
    }
    let Some(script) = locate_watchdog_script(&ctx.repo_root) else {
        missing_script_warning();
        return;
    };

    let wd_label = platform::watchdog_label();
    // The SAME resolved domain the daemon job uses (#4130) so the watchdog is
    // bootstrapped where stop.sh will later look for it.
    let wd_domain = platform::launchd_domain();
    let wd_service = format!("{wd_domain}/{wd_label}");
    let wd_plist = PathBuf::from(&ctx.home)
        .join("Library/LaunchAgents")
        .join(format!("{wd_label}.plist"));
    let wd_interval = platform::watchdog_interval_secs();
    let wd_log = ctx.loom_dir.join("logs/daemon-watchdog.log");

    let _ = std::fs::create_dir_all(PathBuf::from(&ctx.home).join("Library/LaunchAgents"));
    let _ = std::fs::create_dir_all(ctx.loom_dir.join("logs"));

    let rendered = render::watchdog_plist(
        &wd_label,
        &script.display().to_string(),
        &ctx.repo_root.display().to_string(),
        &wd_log.display().to_string(),
        &wd_interval,
        &ctx.plist_path_value,
        &ctx.home,
        &ctx.intent_marker.display().to_string(),
        &ctx.socket_path.display().to_string(),
        &ctx.pid_file.display().to_string(),
        &platform::launchd_label(),
    );

    let wd_job_loaded = launchctl_print_ok(&wd_service);
    let installed_matches = std::fs::read_to_string(&wd_plist).is_ok_and(|t| t == rendered);
    if wd_job_loaded && installed_matches {
        out::say(&format!(
            "Watchdog:       {wd_label} (StartInterval {wd_interval}s) → {} (unchanged, already loaded — skipped reload)",
            wd_log.display()
        ));
        return;
    }
    if std::fs::write(&wd_plist, &rendered).is_err() {
        out::warn(&format!("watchdog: could not install {} — skipping.", wd_plist.display()));
        return;
    }
    if wd_job_loaded {
        let _ = run_quiet("launchctl", &["bootout".into(), wd_service.clone()]);
    }
    if run_quiet(
        "launchctl",
        &[
            "bootstrap".into(),
            wd_domain.clone(),
            wd_plist.display().to_string(),
        ],
    ) {
        out::say(&format!(
            "Watchdog:       {wd_label} (StartInterval {wd_interval}s) → {}",
            wd_log.display()
        ));
    } else {
        out::warn(&format!(
            "watchdog: launchctl bootstrap failed for {wd_service} — autonomy-loss detection not active (non-fatal)."
        ));
    }
}

/// `provision_watchdog_job_systemd()`.
///
/// No unchanged-content guard here, unlike the launchd branch: `systemctl
/// --user enable --now` on an ALREADY ACTIVE timer is a no-op job that does not
/// re-trigger `OnBootSec` — empirically verified under #4862, two consecutive
/// calls produced exactly one execution.
pub fn provision_systemd(ctx: &Ctx) {
    if !platform::have("systemctl") {
        out::warn("watchdog: systemctl not found — skipping.");
        return;
    }
    let Some(script) = locate_watchdog_script(&ctx.repo_root) else {
        missing_script_warning();
        return;
    };

    let wd_unit = platform::systemd_watchdog_unit();
    let svc_unit = format!("{wd_unit}.service");
    let timer_unit = format!("{wd_unit}.timer");
    let unit_dir = platform::systemd_unit_dir();
    let svc_path = unit_dir.join(&svc_unit);
    let timer_path = unit_dir.join(&timer_unit);
    let wd_interval = platform::watchdog_interval_secs();
    let wd_log = ctx.loom_dir.join("logs/daemon-watchdog.log");

    let _ = std::fs::create_dir_all(&unit_dir);
    let _ = std::fs::create_dir_all(ctx.loom_dir.join("logs"));

    let svc = render::systemd_watchdog_service(
        &script.display().to_string(),
        &ctx.repo_root.display().to_string(),
        &wd_log.display().to_string(),
        &ctx.plist_path_value,
        &ctx.home,
        &ctx.intent_marker.display().to_string(),
        &ctx.socket_path.display().to_string(),
        &ctx.pid_file.display().to_string(),
    );
    if std::fs::write(&svc_path, svc).is_err() {
        out::warn(&format!("watchdog: could not write {} — skipping.", svc_path.display()));
        return;
    }
    let timer = render::systemd_watchdog_timer(&svc_unit, &wd_interval);
    if std::fs::write(&timer_path, timer).is_err() {
        out::warn(&format!("watchdog: could not write {} — skipping.", timer_path.display()));
        return;
    }

    let _ = run_quiet("systemctl", &["--user".into(), "daemon-reload".into()]);
    if run_quiet(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            timer_unit.clone(),
        ],
    ) {
        out::say(&format!(
            "Watchdog:       {timer_unit} (OnUnitActiveSec {wd_interval}s) → {}",
            wd_log.display()
        ));
    } else {
        out::warn(&format!(
            "watchdog: systemctl --user enable --now failed for {timer_unit} — autonomy-loss detection not active (non-fatal)."
        ));
    }
}

/// `provision_watchdog_job_none()` — the nohup fallback tier has no scheduled-job
/// mechanism at all, so it warns and escalates instead of silently reporting.
pub fn provision_none(ctx: &Ctx) {
    out::warn("watchdog: no scheduled checker on this platform (nohup-fallback Linux / non-systemd host) — skipping (marker+heartbeat still active). Run loom-daemon-watchdog.sh by hand or wire it to cron.");
    escalate_unprovisionable(ctx);
}

/// `escalate_watchdog_unprovisionable()` (#5343 AC4).
///
/// Files ONE tracking issue, deduped by a persistent sentinel, because a
/// one-line stderr warning is exactly the failure #5343 exists to close: a host
/// can run for months with the gap and nothing surfaces it beyond a log line
/// somebody has to go looking for. Best-effort and non-fatal — no
/// `create-issue.sh`, no forge auth and being offline are all swallowed.
pub fn escalate_unprovisionable(ctx: &Ctx) {
    if !ctx.intent_marker.exists() {
        return;
    }
    let sentinel = ctx.loom_dir.join(".watchdog-unprovisionable-escalated");
    if sentinel.is_file() {
        return;
    }

    let mut issue_script = ctx.repo_root.join(".loom/scripts/create-issue.sh");
    if !issue_script.is_file() {
        issue_script = ctx.repo_root.join("defaults/scripts/create-issue.sh");
    }
    if !issue_script.is_file() {
        out::warn("watchdog: no scheduled checker on this platform, and create-issue.sh not found — cannot escalate (#5343).");
        return;
    }

    let hostname_str = hostname();
    let marker = ctx.intent_marker.display();
    let sentinel_s = sentinel.display();
    // The shell built this with an unquoted heredoc whose backticks were
    // backslash-escaped. Here it is a plain string literal — there is no shell
    // and no heredoc, so the #7508 class (an unescaped backtick being
    // command-substituted into an issue filed unattended during an outage)
    // cannot occur structurally rather than by care.
    let body = format!(
        "The autonomy-desired marker at `{marker}` is present on host `{hostname_str}`,\n\
         meaning a loom-daemon is EXPECTED to be running here — but this platform tier (no\n\
         `systemd --user`, no launchd: a plain nohup-backgrounded daemon, or an explicit\n\
         --no-launchd/--no-systemd start) has no OS-level scheduled-job mechanism this tooling\n\
         can provision a watchdog timer onto.\n\
         \n\
         Nothing is scheduled to detect a future daemon death on this host. Auto-provisioning is\n\
         out of scope here (issue #5343 AC4) — mitigate manually: run\n\
         `loom-daemon-watchdog.sh` by hand, wire it to cron, or move this host onto a\n\
         systemd/launchd-managed start.\n\
         \n\
         Filed automatically by the loom-daemon-start.sh watchdog escalation (#5343). Deduped by a\n\
         sentinel file at `{sentinel_s}` — delete it to allow re-filing after a genuine\n\
         reconfiguration."
    );

    let err_log = ctx.loom_dir.join("logs/.watchdog-escalation-err");
    let err_file = std::fs::File::create(&err_log).ok();
    // `--force` skips create-issue.sh's duplicate backstop (#7971): this filing
    // is already deduped by the sentinel, and an alert must not be silenced by
    // a similarity heuristic.
    let status = Command::new(&issue_script)
        .arg("--title")
        .arg(format!(
            "loom-daemon-watchdog cannot be scheduled on {hostname_str} (no systemd/launchd) — crash protection absent"
        ))
        .arg("--body")
        .arg(&body)
        .arg("--label")
        .arg("loom:triage")
        .arg("--force")
        .stdout(Stdio::null())
        .stderr(err_file.map_or(Stdio::null(), Stdio::from))
        .status();

    if status.is_ok_and(|s| s.success()) {
        let _ = std::fs::create_dir_all(&ctx.loom_dir);
        let _ = std::fs::write(
            &sentinel,
            format!("{}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
        );
        out::warn("watchdog: filed a tracking issue for the unprovisionable watchdog gap on this host (#5343 AC4).");
    } else {
        out::warn(&format!(
            "watchdog: could not file a tracking issue for the unprovisionable watchdog gap (create-issue.sh failed — see {}).",
            err_log.display()
        ));
    }
}

/// `heal_watchdog_provisioning_gap()` (#5343).
///
/// Two paths leave the marker present with NO watchdog ever provisioned —
/// `fleet add-worker`'s hand-rolled unit install, and the daemon's own startup
/// marker healing (#4331) — and before this the already-running guard just
/// `exit 0`ed past the provisioning code, so even a deliberate re-run could not
/// close the gap.
///
/// Safe to call unconditionally: marker absent ⇒ nothing was ever desired;
/// marker present and the job already there ⇒ both provisioners are idempotent.
///
/// The platform detection here is duplicated deliberately, not shared with the
/// real detection block, because it must run BEFORE the already-running guard —
/// ahead of where the real block executes.
pub fn heal(ctx: &Ctx, no_launchd: bool, no_systemd: bool) {
    if !ctx.intent_marker.exists() {
        return;
    }

    let mut heal_use_launchd = false;
    if cfg!(target_os = "macos") {
        heal_use_launchd = true;
        if platform::env_says_off("LOOM_DAEMON_LAUNCHD") {
            heal_use_launchd = false;
        }
    }
    if no_launchd {
        heal_use_launchd = false;
    }

    let heal_is_systemd = !heal_use_launchd
        && !platform::env_says_off("LOOM_DAEMON_SYSTEMD")
        && !no_systemd
        && platform::is_linux_systemd();

    if heal_use_launchd {
        provision_launchd(ctx);
    } else if heal_is_systemd {
        provision_systemd(ctx);
    } else {
        escalate_unprovisionable(ctx);
    }
}

fn launchctl_print_ok(service: &str) -> bool {
    run_quiet("launchctl", &["print".into(), service.to_string()])
}

fn run_quiet(prog: &str, args: &[String]) -> bool {
    Command::new(prog)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `$(hostname 2>/dev/null || echo unknown-host)`.
fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_copy_outranks_the_defaults_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let installed = dir.path().join(".loom/scripts/cli");
        let defaults = dir.path().join("defaults/scripts/cli");
        std::fs::create_dir_all(&installed).expect("mkdir");
        std::fs::create_dir_all(&defaults).expect("mkdir");
        std::fs::write(defaults.join("loom-daemon-watchdog.sh"), "d").expect("write");
        assert_eq!(
            locate_watchdog_script(dir.path()),
            Some(defaults.join("loom-daemon-watchdog.sh"))
        );
        std::fs::write(installed.join("loom-daemon-watchdog.sh"), "i").expect("write");
        assert_eq!(
            locate_watchdog_script(dir.path()),
            Some(installed.join("loom-daemon-watchdog.sh"))
        );
    }

    #[test]
    fn escalation_needs_the_marker_and_respects_its_sentinel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loom_dir = dir.path().join(".loom");
        std::fs::create_dir_all(loom_dir.join("logs")).expect("mkdir");
        let ctx = Ctx {
            repo_root: dir.path().to_path_buf(),
            loom_dir: loom_dir.clone(),
            intent_marker: loom_dir.join("autonomy-desired"),
            socket_path: loom_dir.join("s.sock"),
            pid_file: loom_dir.join(".daemon.pid"),
            plist_path_value: "/usr/bin".to_string(),
            home: dir.path().display().to_string(),
        };
        // No marker ⇒ nothing was ever desired, so nothing is filed and no
        // error log appears.
        escalate_unprovisionable(&ctx);
        assert!(!loom_dir.join("logs/.watchdog-escalation-err").exists());

        // Marker + sentinel ⇒ already escalated, still nothing filed.
        std::fs::write(&ctx.intent_marker, "x").expect("write");
        std::fs::write(loom_dir.join(".watchdog-unprovisionable-escalated"), "ts").expect("write");
        escalate_unprovisionable(&ctx);
        assert!(!loom_dir.join("logs/.watchdog-escalation-err").exists());
    }
}
