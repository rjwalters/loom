//! CPU/load headroom term for the autonomous work-finder's dynamic
//! concurrency cap (Issue #3978).
//!
//! [`crate::work_finder::resolve_dynamic_max_concurrent`] bounds the dynamic
//! cap by `min(healthy_tokens × per_token, disk_headroom, configured_max)`.
//! That formula models the token pool and the scratch-volume disk, but not
//! CPU — so when a batch of exhausted tokens resets and the token axis jumps
//! (e.g. 5 accounts recovering pushes it from ~2 to ~14), the daemon can
//! dispatch far more concurrent `cargo build`s than the host has cores for.
//! The result observed in #3978: 2-3 concurrent Rust builds in worktrees
//! starved `build-gate.sh` of CPU badly enough that it hit its own 600s
//! timeout, which the (separate, already-fixed-elsewhere — see #3974) gate
//! then misreported as a verified-red `main`, halting all dispatch.
//!
//! This module adds a fourth axis to the cap: how many *additional*
//! concurrent sweeps the host's CPU can currently absorb, given
//!
//! 1. **static capacity** — `logical_cpus × utilization_target`, the number
//!    of cores this host is willing to dedicate to sweep work (leaving
//!    headroom for the OS, the daemon itself, and the build-gate's own
//!    `cargo` invocations), and
//! 2. **current load** — the 1-minute load average, subtracted from that
//!    capacity so a host that is *already* busy (someone else's build, a
//!    live sweep mid-compile) backs the term off reactively, not just from a
//!    static core count.
//!
//! Dividing the resulting headroom by an estimated per-sweep core cost
//! (`est_cores_per_sweep` — a `cargo build`/`clippy` invocation typically
//! parallelizes across several cores via rustc codegen units) yields how many
//! more concurrent sweeps the host can start right now.
//!
//! # Why shell out / read `/proc` instead of a `sysinfo`-style crate
//!
//! Matches the precedent set by [`crate::disk_headroom`] (shell out to `df`)
//! and [`crate::worktree_root`]: no new crate dependency, OS-native data
//! sources kept small and directly testable via pure parsing functions
//! ([`parse_proc_loadavg`], [`parse_macos_loadavg`]) split from the I/O.
//! Logical CPU count uses `std::thread::available_parallelism` (stdlib, no
//! dependency at all).
//!
//! # Fail-open, not fail-closed
//!
//! A load-average read failure (unsupported platform, missing `sysctl`,
//! unreadable `/proc/loadavg`) returns `None` and this module falls back to
//! the **static** capacity term (no load subtracted) rather than treating the
//! read failure as "host fully loaded". This mirrors
//! [`crate::capacity::token_axis_limit`]'s policy of falling back to the
//! optimistic basis (the raw pool) when no ranking data exists — the absence
//! of a signal is not evidence of unhealthiness. The term itself is floored
//! at `1` (never `0`): like the token axis's "one healthy account is the
//! throughput floor, never a halt" (#3902), a single mis-calibrated or noisy
//! CPU reading must not be able to wedge the whole dynamic cap shut on its
//! own — [`crate::disk_headroom`] and the token axis are the terms allowed to
//! floor to zero (genuine "no room"/"no healthy accounts" signals); CPU is
//! deliberately a soft backoff, not a hard gate.

// `Command`/`Stdio` back only the macOS `sysctl` read path — cfg-gated so a
// Linux build (which reads `/proc/loadavg` directly, no subprocess) doesn't
// warn on an unused import under `-D warnings`.
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

// ============================================================================
// Configuration (env-only, mirrors `LOOM_PER_WORKTREE_GB` in disk_headroom)
// ============================================================================

/// Environment variable overriding the fraction of logical CPUs the dynamic
/// cap is willing to dedicate to sweep work.
pub const UTILIZATION_TARGET_ENV: &str = "LOOM_CPU_UTILIZATION_TARGET";

/// Default CPU utilization target: 75% of logical cores. Leaves headroom for
/// the OS, the daemon process itself, and (crucially, per #3978) the
/// build-gate's own `cargo`/`clippy` invocations so a fully-subscribed sweep
/// fleet doesn't starve the gate outright.
pub const DEFAULT_UTILIZATION_TARGET: f64 = 0.75;

/// Environment variable overriding the estimated CPU cores a single
/// concurrent sweep consumes while its build/test step is running.
pub const EST_CORES_PER_SWEEP_ENV: &str = "LOOM_EST_CORES_PER_SWEEP";

/// Default estimated cores per sweep. `cargo build`/`clippy` parallelize
/// rustc codegen across multiple cores; `2.0` is a conservative estimate for
/// a Rust-heavy repo like this one (not a hard measurement — tunable via
/// [`EST_CORES_PER_SWEEP_ENV`] per repo/host).
pub const DEFAULT_EST_CORES_PER_SWEEP: f64 = 2.0;

/// Resolve [`UTILIZATION_TARGET_ENV`], falling back to
/// [`DEFAULT_UTILIZATION_TARGET`] when unset, unparseable, or outside `(0,
/// 1]` (a value of `0` would collapse capacity to nothing; a value `> 1`
/// would claim more than the whole host).
#[must_use]
pub fn utilization_target() -> f64 {
    std::env::var(UTILIZATION_TARGET_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0 && *f <= 1.0)
        .unwrap_or(DEFAULT_UTILIZATION_TARGET)
}

/// Resolve [`EST_CORES_PER_SWEEP_ENV`], falling back to
/// [`DEFAULT_EST_CORES_PER_SWEEP`] when unset, unparseable, or `<= 0`.
#[must_use]
pub fn est_cores_per_sweep() -> f64 {
    std::env::var(EST_CORES_PER_SWEEP_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0)
        .unwrap_or(DEFAULT_EST_CORES_PER_SWEEP)
}

// ============================================================================
// Logical CPU count
// ============================================================================

/// The host's logical CPU count via `std::thread::available_parallelism`
/// (stdlib, no dependency). Falls back to `1` on the rare platform where the
/// query fails, matching that API's own documented fallback advice.
#[must_use]
pub fn logical_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

// ============================================================================
// Load average — OS-specific read, OS-agnostic parse
// ============================================================================

/// Parse the 1-minute load average from Linux `/proc/loadavg` contents
/// (`"0.52 0.58 0.59 1/812 12345\n"` — the first field is the 1m average).
/// Returns `None` on any malformed/empty input.
#[must_use]
pub fn parse_proc_loadavg(contents: &str) -> Option<f64> {
    contents.split_whitespace().next()?.parse().ok()
}

/// Parse the 1-minute load average from macOS `sysctl -n vm.loadavg` output
/// (`"{ 1.23 2.34 3.45 }\n"` — brace-wrapped, first field is the 1m average).
/// Returns `None` on any malformed/empty input.
#[must_use]
pub fn parse_macos_loadavg(output: &str) -> Option<f64> {
    let trimmed = output.trim().trim_start_matches('{').trim_end_matches('}');
    trimmed.split_whitespace().next()?.parse().ok()
}

/// Read the current 1-minute load average on Linux via `/proc/loadavg`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn read_loadavg_1m() -> Option<f64> {
    let contents = std::fs::read_to_string("/proc/loadavg").ok()?;
    parse_proc_loadavg(&contents)
}

/// Read the current 1-minute load average on macOS via `sysctl -n
/// vm.loadavg` (no `/proc` on Darwin).
#[cfg(target_os = "macos")]
#[must_use]
pub fn read_loadavg_1m() -> Option<f64> {
    let output = Command::new("sysctl")
        .arg("-n")
        .arg("vm.loadavg")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_macos_loadavg(&String::from_utf8_lossy(&output.stdout))
}

/// No known load-average source on other platforms — the caller falls back
/// to the static (load-agnostic) capacity term.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn read_loadavg_1m() -> Option<f64> {
    None
}

// ============================================================================
// The pure headroom term
// ============================================================================

/// Compute the CPU/load headroom concurrency term.
///
/// `capacity = ncpu × utilization_target` is the number of cores this host is
/// willing to dedicate to sweep work. When a current 1-minute load average is
/// available it is subtracted from that capacity (floored at `0.0` — an
/// already-saturated host contributes no headroom rather than going
/// negative); when unavailable, the term falls back to the static capacity
/// unadjusted (fail-open — see module docs). The resulting headroom divided
/// by `est_cores_per_sweep` gives how many additional concurrent sweeps the
/// host can currently absorb, floored to a minimum of `1` so a single noisy
/// or mis-calibrated CPU reading can never by itself wedge the dynamic cap to
/// zero (mirrors the token axis's "never a halt" floor, #3902).
#[must_use]
pub fn cpu_headroom(
    ncpu: usize,
    loadavg_1m: Option<f64>,
    utilization_target: f64,
    est_cores_per_sweep: f64,
) -> usize {
    let capacity = ncpu as f64 * utilization_target;
    let available = loadavg_1m.map_or(capacity, |load| (capacity - load).max(0.0));
    let term = (available / est_cores_per_sweep.max(f64::MIN_POSITIVE)).floor();
    // NaN/negative-infinity can't occur here (both operands are finite and
    // `est_cores_per_sweep` is clamped away from 0), but `max(1.0)` also
    // guards the float→usize cast below against any residual edge case.
    term.max(1.0) as usize
}

/// Resolve the CPU/load headroom term end-to-end from live host inputs
/// ([`logical_cpu_count`], [`read_loadavg_1m`]) and env-configured knobs
/// ([`utilization_target`], [`est_cores_per_sweep`]).
#[must_use]
pub fn cpu_headroom_limit() -> usize {
    cpu_headroom(
        logical_cpu_count(),
        read_loadavg_1m(),
        utilization_target(),
        est_cores_per_sweep(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ------------------------------------------------------------------
    // parse_proc_loadavg
    // ------------------------------------------------------------------

    #[test]
    fn parse_proc_loadavg_reads_first_field() {
        assert_eq!(parse_proc_loadavg("0.52 0.58 0.59 1/812 12345\n"), Some(0.52));
        assert_eq!(parse_proc_loadavg("2.00 1.50 1.00 3/900 1\n"), Some(2.00));
    }

    #[test]
    fn parse_proc_loadavg_malformed_is_none() {
        assert_eq!(parse_proc_loadavg(""), None);
        assert_eq!(parse_proc_loadavg("not-a-number more stuff\n"), None);
    }

    // ------------------------------------------------------------------
    // parse_macos_loadavg
    // ------------------------------------------------------------------

    #[test]
    fn parse_macos_loadavg_strips_braces() {
        assert_eq!(parse_macos_loadavg("{ 1.23 2.34 3.45 }\n"), Some(1.23));
        assert_eq!(parse_macos_loadavg("{ 0.00 0.01 0.05 }"), Some(0.00));
    }

    #[test]
    fn parse_macos_loadavg_malformed_is_none() {
        assert_eq!(parse_macos_loadavg(""), None);
        assert_eq!(parse_macos_loadavg("{ }"), None);
        assert_eq!(parse_macos_loadavg("{ garbage }"), None);
    }

    // ------------------------------------------------------------------
    // cpu_headroom — pure math
    // ------------------------------------------------------------------

    #[test]
    fn cpu_headroom_static_capacity_when_no_load_reading() {
        // 8 cores, 75% target, no load reading -> capacity 6.0 / 2.0 cores per
        // sweep = 3.
        assert_eq!(cpu_headroom(8, None, 0.75, 2.0), 3);
    }

    #[test]
    fn cpu_headroom_subtracts_current_load() {
        // 8 cores, 75% target -> capacity 6.0. Load 4.0 already in use ->
        // 2.0 available / 2.0 per sweep = 1.
        assert_eq!(cpu_headroom(8, Some(4.0), 0.75, 2.0), 1);
    }

    #[test]
    fn cpu_headroom_floors_at_one_when_load_exceeds_capacity() {
        // Host already saturated (load 20 on an 8-core/75% = 6.0 capacity
        // host) -> available would go negative, clamped to 0, but the term
        // itself floors at 1 (soft backoff, never a hard halt).
        assert_eq!(cpu_headroom(8, Some(20.0), 0.75, 2.0), 1);
    }

    #[test]
    fn cpu_headroom_scales_with_more_cores() {
        // 32 cores, 75% target, no load -> 24.0 / 2.0 = 12.
        assert_eq!(cpu_headroom(32, None, 0.75, 2.0), 12);
    }

    #[test]
    fn cpu_headroom_never_zero_even_on_a_single_core_host() {
        // 1 core, 75% target -> capacity 0.75, / 2.0 per sweep = 0.375 ->
        // floors to 0 mathematically, but the `max(1.0)` policy floor kicks
        // in.
        assert_eq!(cpu_headroom(1, None, 0.75, 2.0), 1);
    }

    #[test]
    fn cpu_headroom_est_cores_per_sweep_scales_the_denominator() {
        // 16 cores, 100% target -> capacity 16.0. A heavier 4.0-cores-per-
        // sweep estimate -> 4.
        assert_eq!(cpu_headroom(16, None, 1.0, 4.0), 4);
    }

    // ------------------------------------------------------------------
    // env resolution
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn utilization_target_default_and_override() {
        std::env::remove_var(UTILIZATION_TARGET_ENV);
        assert!((utilization_target() - DEFAULT_UTILIZATION_TARGET).abs() < f64::EPSILON);

        std::env::set_var(UTILIZATION_TARGET_ENV, "0.5");
        assert!((utilization_target() - 0.5).abs() < f64::EPSILON);

        // Out-of-range and unparseable values fall back to the default.
        std::env::set_var(UTILIZATION_TARGET_ENV, "0");
        assert!((utilization_target() - DEFAULT_UTILIZATION_TARGET).abs() < f64::EPSILON);
        std::env::set_var(UTILIZATION_TARGET_ENV, "1.5");
        assert!((utilization_target() - DEFAULT_UTILIZATION_TARGET).abs() < f64::EPSILON);
        std::env::set_var(UTILIZATION_TARGET_ENV, "garbage");
        assert!((utilization_target() - DEFAULT_UTILIZATION_TARGET).abs() < f64::EPSILON);
        std::env::remove_var(UTILIZATION_TARGET_ENV);
    }

    #[test]
    #[serial]
    fn est_cores_per_sweep_default_and_override() {
        std::env::remove_var(EST_CORES_PER_SWEEP_ENV);
        assert!((est_cores_per_sweep() - DEFAULT_EST_CORES_PER_SWEEP).abs() < f64::EPSILON);

        std::env::set_var(EST_CORES_PER_SWEEP_ENV, "3.5");
        assert!((est_cores_per_sweep() - 3.5).abs() < f64::EPSILON);

        // Zero, negative, and unparseable values fall back to the default.
        std::env::set_var(EST_CORES_PER_SWEEP_ENV, "0");
        assert!((est_cores_per_sweep() - DEFAULT_EST_CORES_PER_SWEEP).abs() < f64::EPSILON);
        std::env::set_var(EST_CORES_PER_SWEEP_ENV, "-1");
        assert!((est_cores_per_sweep() - DEFAULT_EST_CORES_PER_SWEEP).abs() < f64::EPSILON);
        std::env::set_var(EST_CORES_PER_SWEEP_ENV, "garbage");
        assert!((est_cores_per_sweep() - DEFAULT_EST_CORES_PER_SWEEP).abs() < f64::EPSILON);
        std::env::remove_var(EST_CORES_PER_SWEEP_ENV);
    }

    // ------------------------------------------------------------------
    // logical_cpu_count — smoke test
    // ------------------------------------------------------------------

    #[test]
    fn logical_cpu_count_returns_a_plausible_value() {
        let n = logical_cpu_count();
        assert!(n >= 1, "must never be 0 (would divide-by-zero downstream)");
        assert!(n < 4096, "sanity bound — no real host has this many logical cores");
    }

    // ------------------------------------------------------------------
    // cpu_headroom_limit — end-to-end smoke test against the real host
    // ------------------------------------------------------------------

    #[test]
    fn cpu_headroom_limit_returns_at_least_one() {
        // Whatever this CI/dev host's load looks like, the policy floor
        // guarantees at least 1.
        assert!(cpu_headroom_limit() >= 1);
    }
}
