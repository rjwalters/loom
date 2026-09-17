//! Host-side autonomy-loss detector for the raw `loom-daemon` process (#4011),
//! ported from `defaults/scripts/cli/loom-daemon-watchdog.sh` (#8086, epic
//! #7810).
//!
//! # The contract this inherits
//!
//! `.loom/scripts/cli/loom-daemon-watchdog.sh` is invoked **by that path** by a
//! launchd/systemd timer, the `loom` dispatcher and operators, so the script
//! name survives as a stub and this module inherits its CLI surface whole: six
//! flags, roughly forty `LOOM_WATCHDOG_*` environment knobs, the
//! `<ts> [<LEVEL>] <msg>` log line shape, and the exit codes its callers branch
//! on (0 healthy / deliberate stop, 1 divergence or state mismatch, 3 liveness
//! undetermined).
//!
//! # Why this port mostly *deletes* logic rather than translating it
//!
//! The shell had to re-derive, in bash, primitives the daemon already owns —
//! and its own comments say so. `resolve_pid_file()` states it "mirrors the
//! daemon's own `daemon_pidfile::resolve_pid_file_path_from` EXACTLY so the two
//! ends can never mean different files", and records that the #5118 incident
//! was possible *only because each side derived its own path*.
//! `parse_etime_secs()` says it "mirrors the Rust probe's `parse_etime` exactly
//! (`daemon_install_state.rs`, #4368) so the two never disagree".
//!
//! Two implementations kept in agreement by comment is the defect source those
//! comments describe. Here they are one implementation: this module calls
//! [`crate::daemon_pidfile`], [`crate::daemon_install_state`] and
//! [`crate::autonomy_marker`] directly, so the agreement is structural.
//!
//! # The equivalence proof
//!
//! `defaults/scripts/tests/test-loom-daemon-watchdog.sh` (206 assertions) and
//! `test-loom-daemon-watchdog-dedup.sh` (27) are retained black-box suites:
//! they invoke the stub **by path** with environment overrides and assert on
//! stdout, the log file and exit codes. They run unchanged against this code.
//!
//! Per #8011 a retained suite is necessary and not sufficient — it proves only
//! what its author thought to write down. See
//! `defaults/docs/verification-recipes.md` §6 for the differential step.

pub mod config;
pub mod consts;
pub mod env;
pub mod liveness;
pub mod locate;
pub mod marker;
pub mod probe;
pub mod report;
pub mod supervisor;

/// The `--help` banner, kept verbatim from the shell's head-comment block.
///
/// It is the CLI contract (the retained suite greps it for the marker /
/// `StartInterval` rationale and for the #4398 and #5944 knob names) and the
/// design record for every incident that shaped this detector. It lives beside
/// the code as prose rather than inside a string literal so it stays readable
/// and diffable.
pub const HELP_BANNER: &str = include_str!("help.txt");

/// Run one watchdog tick, returning the process exit code.
///
/// The codes are contract — a supervisor and the retained suite both branch on
/// them:
///
/// | code | meaning |
/// |---|---|
/// | 0 | healthy, or the marker is absent so nothing is expected |
/// | 1 | a divergence, or a state mismatch between intent and reality |
/// | 2 | usage error (an unknown flag) |
/// | 3 | liveness undetermined — deliberately NOT an outage (#5118) |
#[must_use]
pub fn tick(verbose: bool) -> i32 {
    let paths = config::Paths::from_env();
    let reporter = report::Reporter::new(paths.log.clone(), verbose);
    let state = consts::StateFiles::resolve(&paths.loom_dir);

    if !paths.marker.exists() {
        return marker_absent(&paths, &reporter, &state);
    }

    // TODO(#8086): sections 2-22 and the 50-series.
    0
}

/// Sections 1 and 1b — there is no operator intent on record.
///
/// The marker's lifetime IS that intent, which is why the detector keys on it
/// rather than on "is the pid file / launchd job present". `loom-daemon-stop.sh`
/// boots out the job AND deletes the pid file, so after ANY stop those would be
/// gone — making a deliberately-stopped daemon and a silently-dead one
/// byte-identical. A detector built on them pages on every intentional stop, or
/// never pages at all.
///
/// Absent intent, a daemon that IS running is still worth a word: it is running
/// UNSUPERVISED, and nothing will revive it when it dies.
fn marker_absent(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
) -> i32 {
    let sup = supervisor::resolve_without_marker(
        cfg!(target_os = "macos"),
        supervisor::systemctl_available(),
    );
    let pid_file = marker::resolve_pid_file(
        env::var("LOOM_PID_FILE"),
        None,
        env::var("LOOM_MACHINE_CHECKOUT"),
        env::var("LOOM_WORKSPACE"),
        None,
        Some(paths.loom_dir.clone()),
    );

    let liveness = crate::daemon_install_state::check_liveness_with_systemd(
        sup.use_launchd,
        &sup.label,
        pid_file.as_deref(),
        env::var("LOOM_LAUNCHD_DOMAIN").as_deref(),
        sup.use_systemd.then_some(sup.systemd_unit.as_str()),
    );

    let mut alive = liveness.alive;
    let mut detail = liveness.detail;

    // The out-of-band signals can only ever be a hint here. Ask the socket
    // before concluding anything, because a served socket is authoritative and
    // an absent pid file is not (#5118).
    if !alive {
        let bin = locate::daemon_bin(None);
        let attempt = probe::attempt(&paths.socket_path, bin.as_deref());
        let (verdict, socket_detail) =
            probe::classify(&attempt, &paths.socket_path, probe::probe_timeout_secs());
        if verdict == probe::SocketVerdict::Answered {
            alive = true;
            detail = format!(
                "a daemon ANSWERS on {} ({socket_detail}); the out-of-band signal disagreed: \
                 {detail}",
                paths.socket_path.display()
            );
        }
    }

    if alive {
        reporter.report(
            report::Level::Warn,
            &format!(
                "STATE MISMATCH: no autonomy-desired marker at {}, but a daemon IS running \
                 ({detail}). Crash protection is DISARMED — if this daemon dies the watchdog \
                 will NOT revive it. Heal it by restarting the daemon (it self-heals the marker \
                 at startup, #4331) or re-running ./.loom/scripts/cli/loom-daemon-start.sh; if \
                 the daemon should NOT be running, stop it with \
                 ./.loom/scripts/cli/loom-daemon-stop.sh.",
                paths.marker.display()
            ),
        );
        return 1;
    }

    // A deliberate stop ends any outage episode: the next real start must begin
    // from a clean slate rather than inheriting a spent attempt budget.
    let _ = std::fs::remove_file(&state.recovery);
    reporter.report(
        report::Level::Ok,
        &format!(
            "RULE: marker absent -> deliberate stop, not reviving (#6388): no autonomy-desired \
             marker at {} — no daemon expected; nothing to check.",
            paths.marker.display()
        ),
    );
    0
}
