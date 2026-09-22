//! The three supervisor tiers: launchd, `systemd --user`, and the plain-nohup
//! fallback — plus the post-bootstrap environment verification (#5081).
//!
//! Each tier ends the process. They are written as `-> !`-shaped functions that
//! call [`std::process::exit`] for the same reason the shell `exit 0`/`exit 1`ed
//! at the same points: the exit code is the contract, and threading a return
//! value back up through five call frames only creates a place for one to be
//! dropped.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::render::{self, Mechanism};
use super::{advisories, marker, out, platform, watchdog_job, Ctx};

/// `tail -n 20 "$START_LOG"` inside the `[[ -s ]]` guard, on stderr.
fn dump_start_log(start_log: &Path) {
    let Ok(text) = std::fs::read_to_string(start_log) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    out::say_err(&format!("----- startup output ({}) -----", start_log.display()));
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().skip(lines.len().saturating_sub(20)) {
        out::say_err(line);
    }
    out::say_err("---------------------------------------");
}

fn socket_busy_hint() {
    out::warn("If another daemon is already listening on the socket, stop it first");
    out::warn("(./.loom/scripts/cli/loom-daemon-stop.sh) and retry.");
}

fn stop_hint(machine_mode: bool) {
    if machine_mode {
        out::say("Stop with: loom stop");
    } else {
        out::say("Stop with: ./.loom/scripts/cli/loom-daemon-stop.sh");
    }
}

/// `if env | grep -qE '^(GH_TOKEN|GITEA_TOKEN|FORGE_TOKEN)='; then chmod 600`.
///
/// The token-forwarding loop writes any exported credential straight into the
/// file, and a plain `>` redirect leaves it at the process umask — typically
/// world-readable (#4005).
fn harden_if_credential_bearing(path: &Path) {
    let carries = std::env::vars_os().any(|(k, _)| {
        matches!(k.to_string_lossy().as_ref(), "GH_TOKEN" | "GITEA_TOKEN" | "FORGE_TOKEN")
    });
    if !carries {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Render into a scratch sibling, run the dropped-key merge against whatever is
/// installed, then move it into place — never render directly over the live
/// file, or the comparison has nothing left to compare against.
fn install_rendered(
    dir: &Path,
    prefix: &str,
    installed: &Path,
    text: &str,
    mech: Mechanism,
    force_env: bool,
) -> std::io::Result<()> {
    let tmp = tempfile::Builder::new()
        .prefix(prefix)
        .rand_bytes(6)
        .tempfile_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let merged = render::warn_dropped_env_keys(
        installed,
        &tmp_path.display().to_string(),
        text,
        mech,
        force_env,
    );
    {
        let mut f = tmp.as_file();
        f.write_all(merged.new_text.as_bytes())?;
        f.flush()?;
    }
    tmp.persist(installed).map_err(|e| e.error)?;
    Ok(())
}

/// The macOS launchd LaunchAgent path (#3972).
pub fn start_launchd(ctx: &Ctx) -> ! {
    let label = platform::launchd_label();
    let domain = platform::launchd_domain();
    let service = format!("{domain}/{label}");
    let plist_dir = PathBuf::from(&ctx.home).join("Library/LaunchAgents");
    let plist_file = plist_dir.join(format!("{label}.plist"));
    let _ = std::fs::create_dir_all(&plist_dir);

    let rendered = render::launchd_plist(
        &label,
        &ctx.daemon_bin_str(),
        &ctx.repo_root.display().to_string(),
        &ctx.start_log.display().to_string(),
        &ctx.plist_path_value,
        &ctx.home,
        &super::envh::forwarded_env_pairs(),
    );
    if install_rendered(
        &plist_dir,
        &format!(".{label}.new."),
        &plist_file,
        &rendered,
        Mechanism::Launchd,
        ctx.args.force_env,
    )
    .is_err()
    {
        out::err(&format!("could not install {}", plist_file.display()));
        std::process::exit(1);
    }
    harden_if_credential_bearing(&plist_file);

    out::say(&format!("Launchd label:  {label}"));
    out::say(&format!("Launchd plist:  {}", plist_file.display()));

    // `launchctl bootout` is ASYNCHRONOUS (#5081): it returns before the kernel
    // has finished tearing the old job down, so an immediate `bootstrap` can
    // race that teardown and fail with EIO against a perfectly valid plist.
    if launchctl_ok(&["print", &service]) {
        let _ = launchctl_ok(&["bootout", &service]);
        let settle = num_env("LOOM_DAEMON_BOOTOUT_SETTLE_SECS", 5);
        let deadline = Instant::now() + Duration::from_secs(settle);
        while launchctl_ok(&["print", &service]) {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let max_attempts = num_env("LOOM_DAEMON_BOOTSTRAP_RETRY_ATTEMPTS", 4);
    let retry_sleep = num_env("LOOM_DAEMON_BOOTSTRAP_RETRY_SECS", 2);
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        let out_res = Command::new("launchctl")
            .args(["bootstrap", &domain, &plist_file.display().to_string()])
            .stdout(Stdio::inherit())
            .output();
        match out_res {
            Ok(o) if o.status.success() => break,
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                // Retry ONLY on the async-bootout EIO shape — never on any
                // other failure, which is a genuine plist/permission problem a
                // retry cannot fix.
                if is_eio(&stderr) && attempt < max_attempts {
                    out::warn(&format!(
                        "launchctl bootstrap hit the async-bootout race (EIO) for {service} -- attempt {attempt}/{max_attempts}, settling {retry_sleep}s and retrying (#5081)."
                    ));
                    std::thread::sleep(Duration::from_secs(retry_sleep));
                    continue;
                }
                out::err(&format!(
                    "launchctl bootstrap failed for {service} (attempt {attempt}/{max_attempts}):"
                ));
                eprint!("{stderr}");
                std::process::exit(1);
            }
            Err(_) => {
                out::err(&format!(
                    "launchctl bootstrap failed for {service} (attempt {attempt}/{max_attempts}):"
                ));
                std::process::exit(1);
            }
        }
    }

    // RunAtLoad alone would start it; kickstart -k makes THIS invocation
    // deterministically win rather than racing launchd's own RunAtLoad timing.
    match Command::new("launchctl")
        .args(["kickstart", "-k", &service])
        .stdout(Stdio::inherit())
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            out::err(&format!("launchctl kickstart failed for {service}:"));
            eprint!("{}", String::from_utf8_lossy(&o.stderr));
            std::process::exit(1);
        }
        Err(_) => {
            out::err(&format!("launchctl kickstart failed for {service}:"));
            std::process::exit(1);
        }
    }

    std::thread::sleep(Duration::from_secs(2));

    let daemon_pid = launchctl_job_pid(&service);
    if daemon_pid.is_none_or(|p| !pid_alive(p)) {
        out::err(&format!("loom-daemon did not stay running under launchd ({service})."));
        dump_start_log(&ctx.start_log);
        socket_busy_hint();
        std::process::exit(1);
    }
    let daemon_pid = daemon_pid.unwrap_or_default();

    // A successful bootstrap plus a live pid say nothing about whether the
    // freshly-rendered EnvironmentVariables actually took effect — launchd's own
    // report is the only authoritative source (#5081).
    match verify_launchd_env_applied(&service, &plist_file) {
        EnvVerdict::Match => {}
        EnvVerdict::Mismatch(detail) => {
            out::err(&format!(
                "loom-daemon is running (pid {daemon_pid}) under launchd, but its reported environment does NOT match the freshly-rendered plist -- refusing to report success (#5081)."
            ));
            eprintln!("{detail}");
            std::process::exit(1);
        }
        EnvVerdict::Unverifiable(detail) => {
            out::warn("Could not verify the running job's env against the plist (plutil/jq unavailable?) -- proceeding, but the env change is unconfirmed:");
            for line in detail.lines() {
                out::warn(&format!("  {line}"));
            }
        }
    }

    let _ = std::fs::write(&ctx.pid_file, format!("{daemon_pid}\n"));
    marker::write(&ctx.loom_dir, &ctx.intent_marker, &ctx.intent(true, &label, false, ""));
    watchdog_job::provision_launchd(&ctx.watchdog_ctx());
    out::ok(&format!("loom-daemon started under launchd (pid {daemon_pid}, label {label})."));
    out::say(&format!("PID file: {}", ctx.pid_file.display()));
    out::say(&format!("Intent marker: {}", ctx.intent_marker.display()));
    advisories::print_safehouse_status(&ctx.repo_root);
    if let Some(bin) = &ctx.daemon_bin {
        advisories::print_calibrate_hint(bin, &ctx.repo_root);
    }
    stop_hint(ctx.machine_mode);
    std::process::exit(0);
}

/// The Linux `systemd --user` service path (#4268).
pub fn start_systemd(ctx: &Ctx) -> ! {
    let unit = platform::systemd_unit();
    let unit_dir = platform::systemd_unit_dir();
    let unit_path = platform::systemd_unit_path();
    let _ = std::fs::create_dir_all(&unit_dir);

    let rendered = render::systemd_unit(
        &ctx.daemon_bin_str(),
        &ctx.repo_root.display().to_string(),
        &ctx.start_log.display().to_string(),
        &ctx.plist_path_value,
        &ctx.home,
        &super::envh::forwarded_env_pairs(),
    );
    if install_rendered(
        &unit_dir,
        &format!(".{unit}.new."),
        &unit_path,
        &rendered,
        Mechanism::Systemd,
        ctx.args.force_env,
    )
    .is_err()
    {
        out::err(&format!("could not install {}", unit_path.display()));
        std::process::exit(1);
    }
    harden_if_credential_bearing(&unit_path);

    out::say(&format!("Systemd unit:   {unit}"));
    out::say(&format!("Unit file:      {}", unit_path.display()));

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match Command::new("systemctl")
        .args(["--user", "enable", "--now", &unit])
        .stdout(Stdio::inherit())
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            out::err(&format!("systemctl --user enable --now failed for {unit}:"));
            eprint!("{}", String::from_utf8_lossy(&o.stderr));
            std::process::exit(1);
        }
        Err(_) => {
            out::err(&format!("systemctl --user enable --now failed for {unit}:"));
            std::process::exit(1);
        }
    }

    std::thread::sleep(Duration::from_secs(2));

    let main_pid = Command::new("systemctl")
        .args(["--user", "show", "-p", "MainPID", "--value", &unit])
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let alive =
        !main_pid.is_empty() && main_pid != "0" && main_pid.parse::<i32>().is_ok_and(pid_alive);
    if !alive {
        out::err(&format!("loom-daemon did not stay running under systemd ({unit})."));
        dump_start_log(&ctx.start_log);
        socket_busy_hint();
        std::process::exit(1);
    }

    let _ = std::fs::write(&ctx.pid_file, format!("{main_pid}\n"));
    marker::write(&ctx.loom_dir, &ctx.intent_marker, &ctx.intent(false, "", true, &unit));
    watchdog_job::provision_systemd(&ctx.watchdog_ctx());
    out::ok(&format!("loom-daemon started under systemd (pid {main_pid}, unit {unit})."));
    out::say(&format!("PID file: {}", ctx.pid_file.display()));
    out::say(&format!("Intent marker: {}", ctx.intent_marker.display()));
    advisories::print_safehouse_status(&ctx.repo_root);
    if let Some(bin) = &ctx.daemon_bin {
        advisories::print_calibrate_hint(bin, &ctx.repo_root);
    }
    out::warn("Reboot survival requires lingering: run 'loginctl enable-linger \"$USER\"' once (SSH-only / headless hosts).");
    stop_hint(ctx.machine_mode);
    std::process::exit(0);
}

/// The plain-nohup fallback: a non-systemd Linux host, or an explicit
/// `--no-launchd` / `--no-systemd`.
pub fn start_nohup(ctx: &Ctx) -> ! {
    let Some(bin) = ctx.daemon_bin.clone() else {
        // Unreachable: the binary lookup already exited 1 outside
        // --heal-watchdog-only, which never reaches here.
        out::err("loom-daemon binary not found.");
        std::process::exit(1);
    };
    let log = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&ctx.start_log)
    {
        Ok(f) => f,
        Err(e) => {
            out::err(&format!("could not open {}: {e}", ctx.start_log.display()));
            std::process::exit(1);
        }
    };
    let log_err = match log.try_clone() {
        Ok(f) => f,
        Err(e) => {
            out::err(&format!("could not open {}: {e}", ctx.start_log.display()));
            std::process::exit(1);
        }
    };

    let mut cmd = Command::new(&bin);
    cmd.stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .stdin(Stdio::null());
    // `nohup` sets SIGHUP to SIG_IGN in the child; the redirections it would
    // otherwise add are already explicit above.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            out::err(&format!("could not start {}: {e}", bin.display()));
            std::process::exit(1);
        }
    };
    let daemon_pid = child.id();

    std::thread::sleep(Duration::from_secs(2));

    // `kill -0 "$daemon_pid"` in bash fails here because bash's SIGCHLD
    // handler has already reaped the background job, so the pid is gone.
    // `try_wait` is the same question asked of our own child without the
    // zombie ambiguity a raw `kill(pid, 0)` would have in a process that has
    // not reaped.
    if matches!(child.try_wait(), Ok(Some(_))) {
        out::err(&format!("loom-daemon exited immediately after start (pid {daemon_pid})."));
        dump_start_log(&ctx.start_log);
        socket_busy_hint();
        std::process::exit(1);
    }

    let _ = std::fs::write(&ctx.pid_file, format!("{daemon_pid}\n"));
    marker::write(&ctx.loom_dir, &ctx.intent_marker, &ctx.intent(false, "", false, ""));
    watchdog_job::provision_none(&ctx.watchdog_ctx());
    out::ok(&format!(
        "loom-daemon started (pid {daemon_pid}). PID file: {}",
        ctx.pid_file.display()
    ));
    out::say(&format!("Intent marker: {}", ctx.intent_marker.display()));
    advisories::print_safehouse_status(&ctx.repo_root);
    advisories::print_calibrate_hint(&bin, &ctx.repo_root);
    stop_hint(ctx.machine_mode);
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// launchd environment verification (#5081, lib/daemon-env-harvest.sh)
// ---------------------------------------------------------------------------

/// The three outcomes `verify_launchd_env_applied` distinguished by exit code.
pub enum EnvVerdict {
    Match,
    /// Exit 1 — a real disagreement, or the job could not be printed at all.
    Mismatch(String),
    /// Exit 2 — `plutil`/`jq` unavailable, or the plist is unparseable.
    Unverifiable(String),
}

/// `verify_launchd_env_applied <service> <plist>`.
///
/// The `plutil`+`jq` requirement is kept rather than replaced with a native
/// read: on a host missing either, the shell reported "unconfirmed" and
/// proceeded, and a port that started verifying there would newly be able to
/// fail a start that used to succeed.
#[must_use]
pub fn verify_launchd_env_applied(service: &str, plist: &Path) -> EnvVerdict {
    let print_output = match Command::new("launchctl")
        .args(["print", service])
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => {
            return EnvVerdict::Mismatch(format!(
                "Cannot verify launchd env: 'launchctl print {service}' failed (job not loaded/running)."
            ))
        }
    };
    launchd_env_matches_plist(plist, &print_output)
}

fn launchd_env_matches_plist(plist: &Path, print_output: &str) -> EnvVerdict {
    if !plist.is_file() {
        return EnvVerdict::Unverifiable(format!(
            "Cannot verify launchd env: plist not found at {}",
            plist.display()
        ));
    }
    if !platform::have("plutil") || !platform::have("jq") {
        return EnvVerdict::Unverifiable(
            "Cannot verify launchd env: plutil and jq are both required on the macOS launchd path."
                .to_string(),
        );
    }
    let Ok(text) = std::fs::read_to_string(plist) else {
        return EnvVerdict::Unverifiable(format!(
            "Cannot verify launchd env: plist at {} is not parseable by plutil.",
            plist.display()
        ));
    };
    let actual = extract_launchd_print_env(print_output);
    let mut problems: Vec<String> = Vec::new();
    for key in render::plist_env_keys(&text) {
        if key.is_empty() {
            continue;
        }
        let expected = render::plist_env_value(&text, &key).unwrap_or_default();
        match actual.iter().find(|(k, _)| k == &key) {
            None => problems.push(format!(
                "launchd env verification: key '{key}' is in the plist but missing from the running job's reported environment."
            )),
            Some((_, got)) if got != &expected => problems.push(format!(
                "launchd env verification: key '{key}' expected [{expected}] but the running job reports [{got}]."
            )),
            Some(_) => {}
        }
    }
    if problems.is_empty() {
        EnvVerdict::Match
    } else {
        EnvVerdict::Mismatch(problems.join("\n"))
    }
}

/// `extract_launchd_print_env <print_output>` — the `environment = { K => V }`
/// block launchd reports for a loaded job.
#[must_use]
pub fn extract_launchd_print_env(print_output: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_env = false;
    for line in print_output.lines() {
        let trimmed = line.trim_start();
        if !in_env {
            if trimmed.starts_with("environment = {") {
                in_env = true;
            }
            continue;
        }
        if line.trim() == "}" {
            in_env = false;
            continue;
        }
        if let Some(idx) = trimmed.find(" => ") {
            out.push((trimmed[..idx].to_string(), trimmed[idx + 4..].to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn launchctl_ok(args: &[&str]) -> bool {
    Command::new("launchctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `awk -F'= ' '/^[[:space:]]*pid = /{gsub(/[^0-9]/, "", $2); print $2; exit}'`.
fn launchctl_job_pid(service: &str) -> Option<i32> {
    let output = Command::new("launchctl")
        .args(["print", service])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if !trimmed.starts_with("pid = ") {
            continue;
        }
        // `-F'= '` splits on the FIRST occurrence for $2's purposes here; the
        // digits-only squeeze then discards anything else on the field.
        let field = line.split_once("= ").map_or("", |x| x.1);
        let digits: String = field.chars().filter(char::is_ascii_digit).collect();
        return digits.parse().ok();
    }
    None
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

fn is_eio(stderr: &str) -> bool {
    // `grep -qE '(^|[^0-9])5: Input/output error'` — the leading guard stops
    // `125: Input/output error` from counting as the EIO shape.
    let needle = "5: Input/output error";
    let mut from = 0;
    while let Some(idx) = stderr[from..].find(needle) {
        let abs = from + idx;
        let prev_ok = abs == 0 || !stderr.as_bytes()[abs - 1].is_ascii_digit();
        if prev_ok {
            return true;
        }
        from = abs + 1;
    }
    false
}

fn num_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eio_guard_rejects_a_digit_prefixed_code() {
        assert!(is_eio("Bootstrap failed: 5: Input/output error"));
        assert!(is_eio("5: Input/output error"));
        assert!(!is_eio("error 125: Input/output error"));
    }

    #[test]
    fn the_launchd_env_block_parses_one_key_per_line() {
        let text = "\tstate = running\n\tenvironment = {\n\t\tPATH => /usr/bin\n\t\tLOOM_WORK_FINDER => 1\n\t}\n\tpid = 42\n";
        let env = extract_launchd_print_env(text);
        assert_eq!(
            env,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("LOOM_WORK_FINDER".to_string(), "1".to_string())
            ]
        );
    }
}
