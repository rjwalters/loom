//! `loom-daemon daemon-start` — the subcommand behind
//! `.loom/scripts/cli/loom-daemon-start.sh` (#8087, epic #7810).
//!
//! # The contract this inherits
//!
//! `scripts/shell-allowlist.txt` records the verdict for that path: *"invoked
//! by that path by the `loom` dispatcher, the daemon, or an operator. The name
//! stays; logic ports behind it."* So the script survives as a stub and this
//! module inherits its whole surface — thirteen flags, twenty-odd `LOOM_*`
//! environment knobs, the exact stdout/stderr split of every banner and
//! advisory, and the exit codes (`0` started or already running, `1` usage /
//! binary not found / failed to start / a refused autonomy downgrade / a
//! refused agent-session start).
//!
//! # Why this port is riskier than the watchdog
//!
//! The watchdog computes a verdict. This **starts a process**, and it has to
//! preserve the FLAGS-OFF / opt-in autonomy contract across that start. A port
//! that brings the daemon up with different effective flags than the shell
//! computed is a silent behavioural change that no "did it start?" assertion
//! can see — the daemon is running, the banner looks right, and dispatch is
//! either quietly off (the 2026-07-30 incident, ~3h of outage) or quietly on.
//!
//! So the equivalence proof has two halves, and the second is not optional:
//!
//! * `defaults/scripts/tests/test-loom-daemon-start.sh` is retained and run
//!   unchanged against this code through the stub.
//! * `loom-daemon/tests/differential_daemon_start.rs` replays a generated
//!   corpus of flag × environment × prior-state combinations against answers
//!   frozen from the pre-port shell, and asserts a mutation-sensitivity floor
//!   rather than a case count. Per #8011 a retained suite proves only what its
//!   author thought to write down.
//!
//! # Module layout
//!
//! | module | the shell it replaces |
//! |---|---|
//! | [`args`] | the argument loop and the persisted-flags filter |
//! | [`autonomy`] | `resolve_autonomy_env`, `warn_autonomy_downgrade` |
//! | [`guards`] | `guard_session_context_start`, `warn_scratch_workdir_drift` |
//! | [`render`] | the four renderers and the extractors/injectors |
//! | [`envh`] | the `env`-plus-`grep` harvest and `xml_escape` |
//! | [`unescape`] | what `printf '%b'` did to the harvested values |
//! | [`marker`] | `write_intent_marker` |
//! | [`watchdog_job`] | the three `provision_watchdog_job_*` tiers + the #5343 heal |
//! | [`advisories`] | `print_safehouse_status`, `print_calibrate_hint`, the host-sleep check |
//! | [`launch`] | the launchd / systemd / nohup start paths |
//! | [`platform`] | the label/domain/unit resolvers the lifecycle scripts share |

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod advisories;
pub mod args;
pub mod autonomy;
pub mod envh;
pub mod guards;
pub mod launch;
pub mod marker;
pub mod out;
pub mod paths;
pub mod platform;
pub mod render;
pub mod unescape;
pub mod watchdog_job;

use args::{Args, Want};
use render::Mechanism;

/// The `--help` banner, kept verbatim from the shell's leading comment block.
///
/// It was recovered by the same `awk` pass `show_help()` used, so `--help` is
/// byte-identical to the pre-port output. #7794 narrowed a torn-banner race by
/// reading `"$0"` once at startup and noted that the permanent fix was this
/// port: there is no longer a file to read, so the race is gone rather than
/// narrowed.
pub const HELP_BANNER: &str = include_str!("help.txt");

/// Everything resolved once, before any branch.
pub struct Ctx {
    pub args: Args,
    pub repo_root: PathBuf,
    pub machine_mode: bool,
    pub state_home: PathBuf,
    pub daemon_bin: Option<PathBuf>,
    pub plist_path_value: String,
    pub pid_file: PathBuf,
    pub socket_path: PathBuf,
    pub start_log: PathBuf,
    pub loom_dir: PathBuf,
    pub intent_marker: PathBuf,
    pub heartbeat_file: PathBuf,
    pub heartbeat_interval_secs: String,
    pub home: String,
    pub argv0: String,
    pub pre_exported_work_finder: String,
    pub pre_exported_health_gate: String,
}

impl Ctx {
    fn daemon_bin_str(&self) -> String {
        self.daemon_bin
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    fn intent<'a>(
        &'a self,
        use_launchd: bool,
        launchd_label: &'a str,
        use_systemd: bool,
        systemd_unit: &'a str,
    ) -> marker::IntentMarker<'a> {
        marker::IntentMarker {
            repo_root: &self.repo_root,
            pid_file: &self.pid_file,
            heartbeat_file: &self.heartbeat_file,
            heartbeat_interval_secs: &self.heartbeat_interval_secs,
            use_launchd,
            launchd_label,
            use_systemd,
            systemd_unit,
            socket_path: &self.socket_path,
        }
    }

    fn watchdog_ctx(&self) -> watchdog_job::Ctx {
        watchdog_job::Ctx {
            repo_root: self.repo_root.clone(),
            loom_dir: self.loom_dir.clone(),
            intent_marker: self.intent_marker.clone(),
            socket_path: self.socket_path.clone(),
            pid_file: self.pid_file.clone(),
            plist_path_value: self.plist_path_value.clone(),
            home: self.home.clone(),
        }
    }
}

/// Run the whole start flow. Never returns.
///
/// `argv` is the argument list **without** the program name; `argv0` is what
/// the refusal messages should tell the operator to re-run, which the stub
/// supplies so they still name the script path an operator typed.
pub fn run(argv: &[String], argv0: &str) -> ! {
    let parsed = match args::parse(argv) {
        args::Parsed::Help => {
            print!("{HELP_BANNER}");
            std::process::exit(0);
        }
        args::Parsed::Unknown(arg) => {
            out::err(&format!("Unknown option '{arg}'"));
            out::say_err("Use --help for usage");
            std::process::exit(1);
        }
        args::Parsed::Args(a) => *a,
    };

    // Snapshot what the CALLING SHELL already exported, BEFORE the autonomy
    // block applies the FLAGS-OFF default: the downgrade check must tell "this
    // invocation's own default logic produced 0" from "the operator explicitly
    // exported 0 themselves".
    let pre_exported_work_finder = std::env::var("LOOM_WORK_FINDER").unwrap_or_default();
    let pre_exported_health_gate = std::env::var("LOOM_MAIN_HEALTH_GATE").unwrap_or_default();

    let home = std::env::var("HOME").unwrap_or_default();
    let mut repo_root = paths::find_repo_root();

    // Machine mode (#4229): `LOOM_MACHINE_CHECKOUT` is authoritative regardless
    // of $PWD, because the label this script drives is a machine-wide
    // singleton — `loom start` from repo A and repo B must resolve the SAME
    // workdir and pid/flags home.
    let machine_checkout = std::env::var("LOOM_MACHINE_CHECKOUT").unwrap_or_default();
    let machine_mode = !machine_checkout.is_empty();
    let state_home: PathBuf;
    if machine_mode {
        let checkout = PathBuf::from(&machine_checkout);
        if !checkout.is_dir() {
            out::err(&format!("LOOM_MACHINE_CHECKOUT does not exist: {machine_checkout}"));
            std::process::exit(1);
        }
        repo_root = Some(checkout);
        state_home = PathBuf::from(&home).join(".loom");
    } else if let Some(root) = &repo_root {
        state_home = root.join(".loom");
    } else {
        out::err("Not in a Loom workspace (.loom directory not found)");
        std::process::exit(1);
    }
    let repo_root = repo_root.unwrap_or_default();

    // The binary lookup is skipped (never fatal) under --heal-watchdog-only
    // (#5405): that mode never starts, stops or even talks to a daemon.
    let daemon_bin = paths::locate_daemon_bin(&repo_root);
    if daemon_bin.is_none() && !parsed.heal_watchdog_only {
        out::err("loom-daemon binary not found. Checked:");
        for line in paths::bin_search_paths(&repo_root) {
            out::say_err(&format!("  - {line}"));
        }
        out::say_err("Build it (cargo build --release -p loom-daemon), install it to one of the paths above, or set LOOM_DAEMON_BIN=/path/to/loom-daemon");
        std::process::exit(1);
    }

    // Resolved ONCE so the daemon plist and the watchdog plist render the
    // identical PATH, and so the choice is logged exactly once per run.
    let plist_path_value = paths::resolve_plist_path();

    // LOOM_PID_FILE is DERIVED-ONLY here BY DESIGN (#6420). Every other end —
    // stop, update, watchdog, daemon_pidfile.rs — honours an inbound value as
    // tier 1; this script is the EXPORTER. Honouring it here would WIDEN what
    // this process touches (it reads the path for the already-running guard,
    // `rm -f`s it, and writes a new pid into it) and LOOM_PID_FILE is ambient
    // in any Loom agent session, so a start inside a scratch fixture would
    // claim and rewrite the LIVE daemon's pid file. Pinned by the retained
    // suite's "LOOM_PID_FILE is an OUTPUT" case.
    let pid_file = state_home.join(".daemon.pid");
    std::env::set_var("LOOM_PID_FILE", &pid_file);

    let socket_path = std::env::var("LOOM_SOCKET_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .map_or_else(|| PathBuf::from(&home).join(".loom/loom-daemon.sock"), PathBuf::from);
    let start_log = state_home.join("logs/daemon-start.log");
    // Skipped for the two pure-inspection modes (#6387): they only render
    // $START_LOG as a STRING into the preview and never open it, so creating
    // the directory would be a filesystem write on a path that advertises "no
    // side effects".
    if !parsed.print_plist && !parsed.print_unit {
        let _ = std::fs::create_dir_all(state_home.join("logs"));
    }

    // LOOM_DIR is the socket's parent, matching the daemon's own
    // resolve_loom_dir(), so pointing LOOM_SOCKET_PATH at a tempdir isolates
    // the marker and heartbeat there too and never touches the real ~/.loom.
    let loom_dir = socket_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let intent_marker = std::env::var("LOOM_AUTONOMY_MARKER")
        .ok()
        .filter(|s| !s.is_empty())
        .map_or_else(|| loom_dir.join("autonomy-desired"), PathBuf::from);
    let heartbeat_file = loom_dir.join("daemon.heartbeat");
    let heartbeat_interval_secs = std::env::var("LOOM_DAEMON_HEARTBEAT_INTERVAL_SECS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "60".to_string());

    let ctx = Ctx {
        args: parsed,
        repo_root,
        machine_mode,
        state_home,
        daemon_bin,
        plist_path_value,
        pid_file,
        socket_path,
        start_log,
        loom_dir,
        intent_marker,
        heartbeat_file,
        heartbeat_interval_secs,
        home,
        argv0: argv0.to_string(),
        pre_exported_work_finder,
        pre_exported_health_gate,
    };

    // --heal-watchdog-only (#5405): a narrow, side-effect-scoped entry point
    // placed BEFORE the already-running guard so it can never fall through into
    // that guard's "stale PID file ⇒ start a NEW daemon" branch.
    if ctx.args.heal_watchdog_only {
        watchdog_job::heal(&ctx.watchdog_ctx(), ctx.args.no_launchd, ctx.args.no_systemd);
        std::process::exit(0);
    }

    // --print-plist / --print-unit (#6387): decided from ARGV ALONE and
    // returning here, BEFORE the already-running guard and therefore before any
    // state read that can branch into provisioning, marker writes or a
    // launchctl/systemctl call. Placement is the whole fix — these two exits
    // used to sit ~300 lines further down, where a live PID file made the guard
    // fire first and bootstrap a REAL watchdog job from a mode documented as
    // having no side effects.
    if ctx.args.print_plist || ctx.args.print_unit {
        autonomy::resolve_autonomy_env(
            &ctx.repo_root,
            ctx.args.from_config,
            ctx.args.want_work_finder,
            ctx.args.want_health_gate,
            true,
        );
        run_inspection_mode_and_exit(&ctx);
    }

    already_running_guard(&ctx);

    advisories::run_host_sleep_check(&ctx.repo_root);

    // Host-sleep prevention, FOREGROUND ONLY (#6311). Deliberately not wired
    // into the systemd or nohup launches: both persist `$daemon_pid` into a
    // file every other lifecycle script assumes IS the daemon's own pid, and
    // prefixing either with `systemd-inhibit` would make that pid belong to
    // `systemd-inhibit` instead.
    let sleep_inhibit_wrap = resolve_sleep_inhibit_wrap(&ctx.repo_root);

    autonomy::resolve_autonomy_env(
        &ctx.repo_root,
        ctx.args.from_config,
        ctx.args.want_work_finder,
        ctx.args.want_health_gate,
        false,
    );

    persist_invocation_flags(&ctx);

    out::say(&format!("Daemon binary: {}", ctx.daemon_bin_str()));
    out::say(&format!("Socket:        {}", ctx.socket_path.display()));
    out::say(&format!("Daemon log:    {}/.loom/daemon.log", ctx.home));
    if ctx.machine_mode {
        out::say(&format!(
            "Mode:          machine (workdir: {}, state: {})",
            ctx.repo_root.display(),
            ctx.state_home.display()
        ));
    } else {
        out::say(&format!("Mode:          dev (repo: {})", ctx.repo_root.display()));
    }

    if ctx.args.foreground {
        out::say("Starting loom-daemon in the foreground (Ctrl-C to stop)...");
        exec_foreground(&ctx, &sleep_inhibit_wrap);
    }

    // ---------- platform detection (#3972 / #4268) ----------
    let is_darwin = cfg!(target_os = "macos");
    let mut use_launchd = false;
    if is_darwin {
        use_launchd = true;
        if platform::env_says_off("LOOM_DAEMON_LAUNCHD") {
            use_launchd = false;
        }
    }
    if ctx.args.no_launchd {
        use_launchd = false;
    }

    let mut is_linux_systemd = false;
    if !use_launchd && !platform::env_says_off("LOOM_DAEMON_SYSTEMD") && !ctx.args.no_systemd {
        if platform::is_linux_systemd() {
            is_linux_systemd = true;
        } else if !is_darwin
            && platform::have("systemctl")
            && !platform::systemd_user_manager_reachable()
        {
            out::warn("systemd --user manager unreachable (no XDG_RUNTIME_DIR / offline) — falling back to nohup.");
            out::warn("For a supervised, reboot-surviving daemon, run: loginctl enable-linger \"$USER\" and retry.");
        }
    }

    // The REAL-START prior-installed file. Left empty on the nohup tier, where
    // no rendered file exists and the marker is the only signal.
    let prior = if use_launchd {
        autonomy::PriorAutonomy {
            file: Some(
                PathBuf::from(&ctx.home)
                    .join("Library/LaunchAgents")
                    .join(format!("{}.plist", platform::launchd_label())),
            ),
            mechanism: Some(Mechanism::Launchd),
        }
    } else if is_linux_systemd {
        autonomy::PriorAutonomy {
            file: Some(platform::systemd_unit_path()),
            mechanism: Some(Mechanism::Systemd),
        }
    } else {
        autonomy::PriorAutonomy {
            file: None,
            mechanism: None,
        }
    };

    let downgrade = autonomy::warn_autonomy_downgrade(&autonomy::DowngradeCheck {
        from_config: ctx.args.from_config,
        work_finder: (
            &std::env::var("LOOM_WORK_FINDER").unwrap_or_default(),
            &ctx.pre_exported_work_finder,
        ),
        health_gate: (
            &std::env::var("LOOM_MAIN_HEALTH_GATE").unwrap_or_default(),
            &ctx.pre_exported_health_gate,
        ),
        want_work_finder: ctx.args.want_work_finder,
        want_health_gate: ctx.args.want_health_gate,
        prior: &prior,
        intent_marker: &ctx.intent_marker,
    });
    if downgrade {
        autonomy::print_downgrade_refusal();
        std::process::exit(1);
    }
    // The drift warning runs FIRST so a refused start still reports both
    // diagnoses.
    guards::warn_scratch_workdir_drift(&ctx.repo_root);
    if matches!(
        guards::guard_session_context_start(
            false,
            false,
            use_launchd,
            is_linux_systemd,
            &ctx.argv0
        ),
        guards::SessionGuard::Refuse
    ) {
        std::process::exit(1);
    }

    let _ = std::fs::write(&ctx.start_log, "");

    if use_launchd && !platform::have("launchctl") {
        out::warn("launchctl not found despite running on Darwin -- falling back to nohup.");
        use_launchd = false;
    }

    if use_launchd {
        launch::start_launchd(&ctx);
    }
    if is_linux_systemd {
        launch::start_systemd(&ctx);
    }
    launch::start_nohup(&ctx);
}

/// The already-running guard, and the #5343 heal it performs on the way out.
fn already_running_guard(ctx: &Ctx) {
    if !ctx.pid_file.is_file() {
        return;
    }
    let existing = std::fs::read_to_string(&ctx.pid_file).unwrap_or_default();
    let existing = existing.trim().to_string();
    let alive = existing
        .parse::<i32>()
        .is_ok_and(|p| p > 0 && unsafe { libc::kill(p, 0) } == 0);
    if !alive {
        // Stale PID file — clean it up and continue.
        let _ = std::fs::remove_file(&ctx.pid_file);
        return;
    }

    out::warn(&format!(
        "loom-daemon already running (pid {existing}, per {}).",
        ctx.pid_file.display()
    ));
    // #5409 secondary papercut: flags passed to THIS invocation are silently
    // ignored on this path, because the daemon is never touched. Say so rather
    // than letting an operator believe the flag applied.
    let mut ignored: Vec<&str> = Vec::new();
    match ctx.args.want_work_finder {
        Want::On => ignored.push("--work-finder"),
        Want::Off => ignored.push("--no-work-finder"),
        Want::Unset => {}
    }
    match ctx.args.want_health_gate {
        Want::On => ignored.push("--health-gate"),
        Want::Off => ignored.push("--no-health-gate"),
        Want::Unset => {}
    }
    if ctx.args.from_config {
        ignored.push("--from-config");
    }
    if !ignored.is_empty() {
        out::warn(&format!(
            // `"$(IFS=', '; echo "${ignored_flags[*]}")"` joins with the FIRST
            // character of `$IFS` only — see the identical note in
            // `autonomy::resolve_autonomy_env`.
            "Ignoring {} -- the daemon is already running, and flags only",
            ignored.join(",")
        ));
        out::warn("take effect on (re)start. To apply them, stop first:");
    }
    // #5343: self-heal a watchdog-provisioning gap even though this invocation
    // is about to exit without touching the running daemon.
    watchdog_job::heal(&ctx.watchdog_ctx(), ctx.args.no_launchd, ctx.args.no_systemd);
    if ctx.machine_mode {
        out::say_err("To restart: loom restart  (or: loom stop && loom start)");
    } else {
        out::say_err(&format!(
            "To restart: ./.loom/scripts/cli/loom-daemon-stop.sh && {}",
            ctx.argv0
        ));
    }
    std::process::exit(0);
}

/// `--print-plist` / `--print-unit` — read-only previews with no side effects.
fn run_inspection_mode_and_exit(ctx: &Ctx) -> ! {
    // The mechanism is decided by the INVOCATION, never by the host OS: these
    // render (and inspect) their mechanism's file regardless of the platform
    // running them. That argv-only decision is what lets this whole block run
    // before platform detection, and therefore before the already-running
    // guard (#6387).
    let prior = if ctx.args.print_plist {
        autonomy::PriorAutonomy {
            file: Some(
                PathBuf::from(&ctx.home)
                    .join("Library/LaunchAgents")
                    .join(format!("{}.plist", platform::launchd_label())),
            ),
            mechanism: Some(Mechanism::Launchd),
        }
    } else {
        autonomy::PriorAutonomy {
            file: Some(platform::systemd_unit_path()),
            mechanism: Some(Mechanism::Systemd),
        }
    };

    // Warn-only by construction on this path, so an operator sees the warning
    // whether they are inspecting or actually starting.
    let _ = autonomy::warn_autonomy_downgrade(&autonomy::DowngradeCheck {
        from_config: ctx.args.from_config,
        work_finder: (
            &std::env::var("LOOM_WORK_FINDER").unwrap_or_default(),
            &ctx.pre_exported_work_finder,
        ),
        health_gate: (
            &std::env::var("LOOM_MAIN_HEALTH_GATE").unwrap_or_default(),
            &ctx.pre_exported_health_gate,
        ),
        want_work_finder: ctx.args.want_work_finder,
        want_health_gate: ctx.args.want_health_gate,
        prior: &prior,
        intent_marker: &ctx.intent_marker,
    });
    guards::warn_scratch_workdir_drift(&ctx.repo_root);
    let _ = guards::guard_session_context_start(
        ctx.args.print_plist,
        ctx.args.print_unit,
        false,
        false,
        &ctx.argv0,
    );

    let tmp_dir = std::env::var("TMPDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);

    if ctx.args.print_plist {
        let label = platform::launchd_label();
        let rendered = render::launchd_plist(
            &label,
            &ctx.daemon_bin_str(),
            &ctx.repo_root.display().to_string(),
            &ctx.start_log.display().to_string(),
            &ctx.plist_path_value,
            &ctx.home,
            &envh::forwarded_env_pairs(),
        );
        // Rendered into a scratch name (never printed directly) so the
        // carry-forward merge can widen it BEFORE printing — the preview must
        // match what a real install would write, not the pre-merge render.
        let tmp_label = tmp_dir
            .join("loom-print-plist.XXXXXX")
            .display()
            .to_string();
        let live_plist = PathBuf::from(&ctx.home)
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        let mut text = rendered;
        if live_plist.is_file() {
            if let Ok(live) = std::fs::read_to_string(&live_plist) {
                if let Some(live_path) = render::plist_path_value(&live) {
                    if !live_path.is_empty() && live_path != ctx.plist_path_value {
                        out::say_err("");
                        out::say_err(&format!(
                            "PATH DRIFT DETECTED vs the installed plist ({}):",
                            live_plist.display()
                        ));
                        out::say_err(&format!("- live: {live_path}"));
                        out::say_err(&format!("+ new:  {}", ctx.plist_path_value));
                    }
                }
            }
            text = render::warn_dropped_env_keys(
                &live_plist,
                &tmp_label,
                &text,
                Mechanism::Launchd,
                ctx.args.force_env,
            )
            .new_text;
        }
        print!("{text}");
        std::process::exit(0);
    }

    let rendered = render::systemd_unit(
        &ctx.daemon_bin_str(),
        &ctx.repo_root.display().to_string(),
        &ctx.start_log.display().to_string(),
        &ctx.plist_path_value,
        &ctx.home,
        &envh::forwarded_env_pairs(),
    );
    let tmp_label = tmp_dir.join("loom-print-unit.XXXXXX").display().to_string();
    let mut text = rendered;
    if let Some(live_unit) = &prior.file {
        if live_unit.is_file() {
            text = render::warn_dropped_env_keys(
                live_unit,
                &tmp_label,
                &text,
                Mechanism::Systemd,
                ctx.args.force_env,
            )
            .new_text;
        }
    }
    print!("{text}");
    std::process::exit(0);
}

/// Persist the autonomy subset of this invocation (#3968).
///
/// Written on every start attempt, success or failure, so the record always
/// reflects the most recent invocation — `loom-daemon-update.sh` replays it
/// after a rebuild so the FLAGS-OFF/opt-in contract never widens across an
/// update.
fn persist_invocation_flags(ctx: &Ctx) {
    let flags_file = ctx.state_home.join(".daemon.flags");
    let body = args::persisted_flags(&ctx.args.original)
        .into_iter()
        .map(|f| format!("{f}\n"))
        .collect::<String>();
    let _ = std::fs::write(&flags_file, body);
}

/// `DAEMON_SLEEP_INHIBIT_WRAP` — the `systemd-inhibit` prefix, or empty.
///
/// The probe (`systemd-inhibit … -- true`) is kept: on a host where the tool
/// exists but the inhibit cannot be taken, wrapping would fail the exec
/// outright, and this knob must never block a start.
fn resolve_sleep_inhibit_wrap(repo_root: &Path) -> Vec<String> {
    if !advisories::host_prevent_sleep_enabled(repo_root) {
        return Vec::new();
    }
    if !platform::have("systemd-inhibit") {
        return Vec::new();
    }
    let probe = Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep",
            "--who=loom",
            "--why=probe",
            "--",
            "true",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !probe {
        return Vec::new();
    }
    out::say("Sleep inhibit:  host.preventSleep enabled — foreground mode will wrap in systemd-inhibit (issue #6311)");
    vec![
        "systemd-inhibit".to_string(),
        "--what=idle:sleep".to_string(),
        "--who=loom".to_string(),
        "--why=daemon".to_string(),
        "--".to_string(),
    ]
}

/// `exec ${DAEMON_SLEEP_INHIBIT_WRAP[@]+"…"} "$DAEMON_BIN"`.
///
/// A real `exec`, not a spawn-and-wait: `--foreground` is Ctrl-C-driven and the
/// daemon must own the terminal, the process group and the exit status
/// directly. Replacing it with a wait would put this process between the
/// operator's SIGINT and the daemon.
fn exec_foreground(ctx: &Ctx, wrap: &[String]) -> ! {
    let Some(bin) = &ctx.daemon_bin else {
        out::err("loom-daemon binary not found.");
        std::process::exit(1);
    };
    let (prog, rest): (String, Vec<String>) = if wrap.is_empty() {
        (bin.display().to_string(), Vec::new())
    } else {
        (
            wrap[0].clone(),
            wrap[1..]
                .iter()
                .cloned()
                .chain(std::iter::once(bin.display().to_string()))
                .collect(),
        )
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(&prog).args(&rest).exec();
        out::err(&format!("exec {prog} failed: {err}"));
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(&prog).args(&rest).status();
        std::process::exit(status.map_or(1, |s| s.code().unwrap_or(1)));
    }
}
