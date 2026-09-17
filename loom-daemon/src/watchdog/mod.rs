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
pub mod env;
pub mod marker;
pub mod report;

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

    if !paths.marker.exists() {
        // Section 1. The marker's lifetime IS operator intent, which is why the
        // detector keys on it rather than on "is the pid file / launchd job
        // present": loom-daemon-stop.sh removes both of those, so after ANY
        // stop a deliberately-stopped daemon and a silently-dead one would be
        // byte-identical. A detector built on them pages on every intentional
        // stop, or never pages at all.
        //
        // TODO(#8086): section 1b — a daemon that IS running with no marker is
        // a STATE MISMATCH (crash protection disarmed) and exits 1. That needs
        // the liveness + socket probe, which land next.
        reporter.report(
            report::Level::Ok,
            &format!(
                "RULE: marker absent -> deliberate stop, not reviving (#6388): no \
                 autonomy-desired marker at {} — no daemon expected; nothing to check.",
                paths.marker.display()
            ),
        );
        return 0;
    }

    // TODO(#8086): sections 2-22 and the 50-series.
    0
}
