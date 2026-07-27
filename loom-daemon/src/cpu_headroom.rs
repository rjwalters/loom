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
//! 2. **current CPU consumption** — `logical_cpus × (1 − idle_fraction)`,
//!    the number of cores actually being *consumed* right now, subtracted
//!    from that capacity so a host that is already busy (someone else's
//!    build, a live sweep mid-compile) backs the term off reactively.
//!
//! Dividing the resulting headroom by an estimated per-sweep core cost
//! (`est_cores_per_sweep` — a `cargo build`/`clippy` invocation typically
//! parallelizes across several cores via rustc codegen units) yields how many
//! more concurrent sweeps the host can start right now.
//!
//! # Why measured idle fraction, not the load average (#4031)
//!
//! The original #3978 term used the **1-minute load average** as a stand-in
//! for consumed cores. On macOS that overstates consumption by ~1.5×: an
//! observed idle-but-loaded host showed `load1m ≈ 6–7` alongside 76–86% CPU
//! idle on 28 cores — only ~4–7 cores actually consumed. Load average counts
//! threads in the runnable **and** uninterruptible-sleep states, and this
//! daemon's workload is dominated by `claude` sessions **blocked on network
//! I/O**, which inflate load without consuming a core. That pointed the
//! feedback loop the wrong way: more concurrent sweeps → more blocked `claude`
//! processes → higher load average → *lower* concurrency cap, even while the
//! CPU sat idle. This module now derives `consumed_cores` from a **measured
//! idle fraction** (actual CPU consumption), per-platform, and keeps the load
//! average only as a fail-open fallback.
//!
//! # Why shell out / read `/proc` instead of a `sysinfo`-style crate
//!
//! Matches the precedent set by [`crate::disk_headroom`] (shell out to `df`)
//! and [`crate::worktree_root`]: no new crate dependency, OS-native data
//! sources kept small and directly testable via pure parsing functions
//! ([`parse_proc_loadavg`], [`parse_macos_loadavg`], [`parse_proc_stat_cpu`],
//! [`parse_iostat_cpu`]) split from the I/O. Logical CPU count uses
//! `std::thread::available_parallelism` (stdlib, no dependency at all).
//!
//! ## Per-platform idle measurement, and the blocking-call hazard
//!
//! - **Linux** deltas two reads of the aggregate `cpu` line in `/proc/stat`
//!   (`idle + iowait` vs total). The previous cumulative sample is memoized in
//!   a process-global cache and deltaed **across ticks**, so nothing ever
//!   sleeps.
//! - **macOS** shells to `iostat -c 2 -w 1 -n 0`: the **second** data line is
//!   a genuine 1-second delta (the first is cumulative since boot and must not
//!   be used), `id` is idle %. That 1-second wait would block a tokio runtime
//!   worker if called inline from a `ticker.tick().await` loop, so the read is
//!   moved to `spawn_blocking` at the call sites and its result is **memoized**
//!   behind a short TTL ([`CPU_UTIL_MEMO_TTL`]). The memoized cache is shared
//!   by the work-finder loops and the synchronous `ipc.rs` status path, so a
//!   status request and a work-finder tick never each pay the full second.
//!
//! # Fail-open, not fail-closed
//!
//! The signal chain is **measured idle fraction → 1-minute load average →
//! static capacity**, each stage falling forward to the next when its input is
//! unavailable. An unreadable utilization signal (no `/proc/stat`, missing
//! `iostat`, unsupported platform, or simply no delta sampled yet) falls back
//! to the load average (#3978's behavior); an unreadable load average in turn
//! falls back to the **static** capacity term (no consumption subtracted)
//! rather than treating the read failure as "host fully loaded". This mirrors
//! [`crate::capacity::token_axis_limit`]'s policy of falling back to the
//! optimistic basis (the raw pool) when no ranking data exists — the absence
//! of a signal is not evidence of unhealthiness. The term itself is floored
//! at `1` (never `0`): like the token axis's "one healthy account is the
//! throughput floor, never a halt" (#3902), a single mis-calibrated or noisy
//! CPU reading must not be able to wedge the whole dynamic cap shut on its
//! own — [`crate::disk_headroom`] and the token axis are the terms allowed to
//! floor to zero (genuine "no room"/"no healthy accounts" signals); CPU is
//! deliberately a soft backoff, not a hard gate.

use std::sync::Mutex;
use std::time::{Duration, Instant};

// `Command`/`Stdio` back only the macOS `sysctl` / `iostat` read paths —
// cfg-gated so a Linux build (which reads `/proc/*` directly, no subprocess)
// doesn't warn on an unused import under `-D warnings`.
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

// ============================================================================
// Configuration (env > config > default — #4032)
// ============================================================================
//
// Both knobs resolve **env > `.loom/config.json` → `autonomous.*` > default**,
// matching every other autonomous-dispatch knob (`workFinder`,
// `mainHealthGate`, `perTokenConcurrency`, `tokenRankingRefresh`,
// `roleRunner`). Resolution is single-root and startup-time — read once from
// the daemon's primary workspace via [`crate::work_finder::WorkFinderConfig`]
// and threaded through as plain `f64`s — exactly like
// [`crate::work_finder::resolve_per_token_concurrency`], not per-workspace
// (the dynamic cap is one global number per tick; see
// `crate::work_finder::spawn_multi_work_finder_task`'s module docs).
//
// `LOOM_PER_WORKTREE_GB` in [`crate::disk_headroom`] deliberately does **not**
// get the same treatment here (#4032 decision, recorded rather than
// implemented): it has a second, independent Bash-side reader
// (`defaults/scripts/lib/disk-headroom.sh`, consumed by `/loom:sweep` Stage -1
// wave sizing). Migrating only the Rust side would make the same env var honor
// `.loom/config.json` on the daemon path while silently ignoring it on the
// sweep path — a worse, divergent state than today's consistent env-only
// behavior. Moving both means wiring the Bash `config-resolver.sh` into
// `disk-headroom.sh` too, which is a separate, larger change. File a follow-up
// if that cost is judged worth paying.

/// Environment variable overriding the fraction of logical CPUs the dynamic
/// cap is willing to dedicate to sweep work. Wins over
/// `autonomous.cpuUtilizationTarget`.
pub const UTILIZATION_TARGET_ENV: &str = "LOOM_CPU_UTILIZATION_TARGET";

/// Default CPU utilization target: 75% of logical cores. Leaves headroom for
/// the OS, the daemon process itself, and (crucially, per #3978) the
/// build-gate's own `cargo`/`clippy` invocations so a fully-subscribed sweep
/// fleet doesn't starve the gate outright.
pub const DEFAULT_UTILIZATION_TARGET: f64 = 0.75;

/// Environment variable overriding the estimated CPU cores a single
/// concurrent sweep consumes while its build/test step is running. Wins over
/// `autonomous.estCoresPerSweep`.
pub const EST_CORES_PER_SWEEP_ENV: &str = "LOOM_EST_CORES_PER_SWEEP";

/// Default estimated cores per sweep. `cargo build`/`clippy` parallelize
/// rustc codegen across multiple cores; `2.0` is a conservative estimate for
/// a Rust-heavy repo like this one (per #4031's measurement, this is now the
/// calibrated figure for the primary host rather than an un-measured guess —
/// still tunable per repo/host via [`EST_CORES_PER_SWEEP_ENV`] or
/// `autonomous.estCoresPerSweep`).
pub const DEFAULT_EST_CORES_PER_SWEEP: f64 = 2.0;

/// Env override for [`UTILIZATION_TARGET_ENV`] — `None` when unset,
/// unparseable, or outside `(0, 1]` (a value of `0` would collapse capacity to
/// nothing; a value `> 1` would claim more than the whole host).
fn env_utilization_target() -> Option<f64> {
    std::env::var(UTILIZATION_TARGET_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0 && *f <= 1.0)
}

/// Env override for [`EST_CORES_PER_SWEEP_ENV`] — `None` when unset,
/// unparseable, or `<= 0`.
fn env_est_cores_per_sweep() -> Option<f64> {
    std::env::var(EST_CORES_PER_SWEEP_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0)
}

/// Resolve the CPU utilization target with precedence **env > config >
/// default**. `config` is the already-range-filtered
/// `autonomous.cpuUtilizationTarget` value from
/// [`crate::work_finder::read_work_finder_config`] (`None` when absent,
/// malformed, or out of `(0, 1]`).
#[must_use]
pub fn resolve_utilization_target(config: Option<f64>) -> f64 {
    env_utilization_target()
        .or(config)
        .unwrap_or(DEFAULT_UTILIZATION_TARGET)
}

/// Resolve the estimated cores-per-sweep with precedence **env > config >
/// default**. `config` is the already-range-filtered
/// `autonomous.estCoresPerSweep` value from
/// [`crate::work_finder::read_work_finder_config`] (`None` when absent,
/// malformed, or `<= 0`).
#[must_use]
pub fn resolve_est_cores_per_sweep(config: Option<f64>) -> f64 {
    env_est_cores_per_sweep()
        .or(config)
        .unwrap_or(DEFAULT_EST_CORES_PER_SWEEP)
}

/// Resolve [`UTILIZATION_TARGET_ENV`] alone, falling back to
/// [`DEFAULT_UTILIZATION_TARGET`] when unset, unparseable, or outside `(0,
/// 1]`. Env-only convenience wrapper around [`resolve_utilization_target`]
/// for callers with no config value in hand (e.g. ad-hoc tooling); production
/// call sites resolve config too and should call [`resolve_utilization_target`]
/// directly.
#[must_use]
pub fn utilization_target() -> f64 {
    resolve_utilization_target(None)
}

/// Resolve [`EST_CORES_PER_SWEEP_ENV`] alone, falling back to
/// [`DEFAULT_EST_CORES_PER_SWEEP`] when unset, unparseable, or `<= 0`.
/// Env-only convenience wrapper around [`resolve_est_cores_per_sweep`] for
/// callers with no config value in hand; production call sites resolve
/// config too and should call [`resolve_est_cores_per_sweep`] directly.
#[must_use]
pub fn est_cores_per_sweep() -> f64 {
    resolve_est_cores_per_sweep(None)
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
// CPU utilization — measured idle fraction (#4031), OS-specific read, pure parse
// ============================================================================

/// A cumulative CPU-time sample from Linux `/proc/stat`'s aggregate `cpu`
/// line. Deltaed against a previous sample to derive the idle fraction over
/// the interval between two reads (no sleep required — see [`CpuUtilState`]).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStatCpu {
    /// Idle jiffies (`idle + iowait`) accumulated since boot.
    pub idle: u64,
    /// Total jiffies across every state accumulated since boot.
    pub total: u64,
}

#[cfg(target_os = "linux")]
impl ProcStatCpu {
    /// The idle fraction over the interval `prev → self`, or `None` when the
    /// total-jiffies delta is zero (two reads too close together / counter
    /// reset). `saturating_sub` guards a wrapped/reset counter from panicking.
    #[must_use]
    pub fn idle_fraction_since(&self, prev: &ProcStatCpu) -> Option<f64> {
        let total_delta = self.total.saturating_sub(prev.total);
        if total_delta == 0 {
            return None;
        }
        let idle_delta = self.idle.saturating_sub(prev.idle);
        Some(idle_delta as f64 / total_delta as f64)
    }
}

/// Parse the aggregate `cpu` line from Linux `/proc/stat` contents into a
/// cumulative [`ProcStatCpu`] sample. The line is
/// `"cpu  user nice system idle iowait irq softirq steal guest guest_nice"`
/// (jiffies since boot); idle counts `idle + iowait`, total sums every field.
/// Returns `None` when no aggregate `cpu ` line is present or it is malformed.
#[cfg(target_os = "linux")]
#[must_use]
pub fn parse_proc_stat_cpu(contents: &str) -> Option<ProcStatCpu> {
    // The aggregate line is the one whose first token is exactly "cpu" (the
    // per-core lines are "cpu0", "cpu1", … and must be skipped).
    let line = contents
        .lines()
        .find(|l| l.split_whitespace().next() == Some("cpu"))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    // Need at least user..iowait (indices 0..=4) to compute idle+iowait.
    if fields.len() < 5 {
        return None;
    }
    let idle = fields[3] + fields[4]; // idle + iowait
    let total: u64 = fields.iter().sum();
    Some(ProcStatCpu { idle, total })
}

/// Parse the idle fraction (`0.0..=1.0`) from macOS `iostat -c 2 -w 1 -n 0`
/// output. Two CPU data lines are emitted: the **first** is cumulative since
/// boot and must be ignored; the **second** is a genuine 1-second delta. Each
/// data line's trailing six columns are `us sy id 1m 5m 15m`, so `id` (idle %)
/// is the fourth-from-last field — a position that holds whether or not disk
/// columns precede it. Returns `None` on malformed input or fewer than two
/// data lines.
#[must_use]
pub fn parse_iostat_cpu(output: &str) -> Option<f64> {
    let mut data_lines = output.lines().filter_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 6 {
            return None;
        }
        // A data line's last six columns (`us sy id 1m 5m 15m`) are all numeric;
        // header lines ("us sy id", "cpu    load average") are not.
        let tail = &tokens[tokens.len() - 6..];
        if tail.iter().all(|t| t.parse::<f64>().is_ok()) {
            tokens[tokens.len() - 4].parse::<f64>().ok()
        } else {
            None
        }
    });
    // Discard the since-boot cumulative line; use the 1-second delta line.
    data_lines.next()?;
    let idle_percent = data_lines.next()?;
    if (0.0..=100.0).contains(&idle_percent) {
        Some(idle_percent / 100.0)
    } else {
        None
    }
}

/// Read the aggregate `/proc/stat` CPU sample on Linux.
#[cfg(target_os = "linux")]
#[must_use]
fn read_proc_stat_cpu() -> Option<ProcStatCpu> {
    let contents = std::fs::read_to_string("/proc/stat").ok()?;
    parse_proc_stat_cpu(&contents)
}

/// Read the current idle fraction on macOS via `iostat -c 2 -w 1 -n 0`.
///
/// **Blocks for ~1 second** (the sampling window). Callers on the tokio
/// runtime MUST route this through `spawn_blocking` + the memoized cache
/// ([`refresh_cpu_util_cache`]); never call it inline from an async task.
#[cfg(target_os = "macos")]
#[must_use]
fn read_iostat_idle_fraction() -> Option<f64> {
    let output = Command::new("iostat")
        .args(["-c", "2", "-w", "1", "-n", "0"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_iostat_cpu(&String::from_utf8_lossy(&output.stdout))
}

// ============================================================================
// Memoized utilization cache (#4031)
// ============================================================================

/// TTL for the memoized idle-fraction sample. A read that would sleep (macOS
/// `iostat`) is taken at most once per this window; within it, both a status
/// request and a work-finder tick reuse the cached value. Also bounds how
/// often the Linux cross-tick `/proc/stat` delta is re-sampled.
pub const CPU_UTIL_MEMO_TTL: Duration = Duration::from_secs(10);

/// Process-global memoized CPU-utilization state.
struct CpuUtilState {
    /// Last successfully measured idle fraction (`0.0..=1.0`), or `None` until
    /// one has been computed (falls back to loadavg meanwhile).
    idle_fraction: Option<f64>,
    /// When the cache was last refreshed (success or attempt), for the TTL gate.
    updated_at: Option<Instant>,
    /// Linux only: the previous cumulative `/proc/stat` sample, deltaed across
    /// refreshes to derive `idle_fraction` without ever sleeping.
    #[cfg(target_os = "linux")]
    prev: Option<ProcStatCpu>,
}

impl CpuUtilState {
    const fn new() -> Self {
        CpuUtilState {
            idle_fraction: None,
            updated_at: None,
            #[cfg(target_os = "linux")]
            prev: None,
        }
    }
}

static CPU_UTIL_STATE: Mutex<CpuUtilState> = Mutex::new(CpuUtilState::new());

/// Refresh the memoized idle-fraction sample.
///
/// A no-op when a sample is younger than [`CPU_UTIL_MEMO_TTL`] (this is the
/// memoization that keeps a burst of status requests + work-finder ticks from
/// each paying the macOS 1-second `iostat`). Otherwise:
///
/// - **Linux** reads `/proc/stat` and deltas it against the previous cached
///   sample (no sleep). The first refresh only seeds the previous sample — the
///   idle fraction stays `None` (loadavg fallback) until a second refresh has a
///   delta to compute.
/// - **macOS** runs `iostat` (~1s wall) and caches the parsed idle fraction.
///
/// **Blocks on macOS.** Call from `spawn_blocking`, never inline on the tokio
/// runtime. Cheap and non-sleeping on Linux, but still routed through
/// `spawn_blocking` at the async call sites for symmetry.
pub fn refresh_cpu_util_cache() {
    let mut st = CPU_UTIL_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(at) = st.updated_at {
        if at.elapsed() < CPU_UTIL_MEMO_TTL {
            return;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(cur) = read_proc_stat_cpu() {
            if let Some(prev) = st.prev {
                if let Some(frac) = cur.idle_fraction_since(&prev) {
                    st.idle_fraction = Some(frac);
                }
            }
            st.prev = Some(cur);
            st.updated_at = Some(Instant::now());
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(frac) = read_iostat_idle_fraction() {
            st.idle_fraction = Some(frac);
        }
        st.updated_at = Some(Instant::now());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        st.updated_at = Some(Instant::now());
    }
}

/// The last memoized idle fraction, or `None` when none has been sampled yet.
/// Never blocks — a pure cache read, safe on the tokio runtime and from the
/// synchronous `ipc.rs` status path.
#[must_use]
pub fn cached_cpu_idle_fraction() -> Option<f64> {
    CPU_UTIL_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .idle_fraction
}

// ============================================================================
// The pure headroom term
// ============================================================================

/// Compute the CPU headroom concurrency term.
///
/// `capacity = ncpu × utilization_target` is the number of cores this host is
/// willing to dedicate to sweep work. From it we subtract the cores currently
/// **consumed**, resolved via the fail-open chain (see module docs):
///
/// 1. a measured `idle_fraction` → `consumed = ncpu × (1 − idle_fraction)`
///    (actual CPU consumption — the #4031 fix), else
/// 2. the 1-minute `loadavg_1m` as a proxy (#3978's behavior), else
/// 3. nothing — the static capacity, unadjusted.
///
/// The consumption is floored at `0.0` (an already-saturated host contributes
/// no headroom rather than going negative). The resulting headroom divided by
/// `est_cores_per_sweep` gives how many additional concurrent sweeps the host
/// can currently absorb, floored to a minimum of `1` so a single noisy or
/// mis-calibrated CPU reading can never by itself wedge the dynamic cap to
/// zero (mirrors the token axis's "never a halt" floor, #3902).
#[must_use]
pub fn cpu_headroom(
    ncpu: usize,
    idle_fraction: Option<f64>,
    loadavg_1m: Option<f64>,
    utilization_target: f64,
    est_cores_per_sweep: f64,
) -> usize {
    let capacity = ncpu as f64 * utilization_target;
    // Prefer the measured idle fraction; fall back to load average; else no
    // consumption term at all (static capacity).
    let consumed = match idle_fraction {
        Some(idle) => Some(ncpu as f64 * (1.0 - idle.clamp(0.0, 1.0))),
        None => loadavg_1m,
    };
    let available = consumed.map_or(capacity, |c| (capacity - c).max(0.0));
    let term = (available / est_cores_per_sweep.max(f64::MIN_POSITIVE)).floor();
    // NaN/negative-infinity can't occur here (all operands are finite and
    // `est_cores_per_sweep` is clamped away from 0), but `max(1.0)` also
    // guards the float→usize cast below against any residual edge case.
    term.max(1.0) as usize
}

/// A point-in-time snapshot of the inputs feeding [`cpu_headroom`], for the
/// status surface (`ipc.rs` / `loom-daemon status`). Reads the memoized idle
/// fraction (no blocking) plus a fresh, fast load-average read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuHeadroomSnapshot {
    /// Host logical CPU count.
    pub logical_cpus: usize,
    /// Measured idle fraction (`0.0..=1.0`) if one has been sampled, else `None`.
    pub idle_fraction: Option<f64>,
    /// Current 1-minute load average if available, else `None`.
    pub loadavg_1m: Option<f64>,
    /// The resulting headroom term (concurrent-sweep slots).
    pub cpu_headroom: usize,
}

/// Build a [`CpuHeadroomSnapshot`] from the memoized idle fraction, a fresh
/// load-average read, and the resolved knobs. **Never blocks** — safe on the
/// tokio runtime and from the synchronous status path. Pre-warm the idle
/// cache with `spawn_blocking(refresh_cpu_util_cache)` for a fresh
/// measurement.
///
/// `utilization_target` / `est_cores_per_sweep` are the already-resolved
/// (env > config > default, #4032) values — callers get these from
/// [`resolve_utilization_target`] / [`resolve_est_cores_per_sweep`] fed by
/// [`crate::work_finder::read_work_finder_config`], resolved once at startup
/// (or, in `ipc.rs`, freshly per status request against the same config path).
#[must_use]
pub fn cpu_headroom_snapshot(
    utilization_target: f64,
    est_cores_per_sweep: f64,
) -> CpuHeadroomSnapshot {
    let logical_cpus = logical_cpu_count();
    let idle_fraction = cached_cpu_idle_fraction();
    let loadavg_1m = read_loadavg_1m();
    let cpu_headroom = cpu_headroom(
        logical_cpus,
        idle_fraction,
        loadavg_1m,
        utilization_target,
        est_cores_per_sweep,
    );
    CpuHeadroomSnapshot {
        logical_cpus,
        idle_fraction,
        loadavg_1m,
        cpu_headroom,
    }
}

/// Resolve the CPU headroom term end-to-end from live host inputs, refreshing
/// the memoized idle-fraction sample first.
///
/// `utilization_target` / `est_cores_per_sweep` are the already-resolved
/// (env > config > default, #4032) values — see [`cpu_headroom_snapshot`].
///
/// **May block** (~1s on macOS via `iostat`; fast, non-sleeping on Linux). The
/// async work-finder loops call this through `spawn_blocking` so the macOS
/// sampling window never blocks a tokio runtime worker.
#[must_use]
pub fn cpu_headroom_limit(utilization_target: f64, est_cores_per_sweep: f64) -> usize {
    refresh_cpu_util_cache();
    cpu_headroom(
        logical_cpu_count(),
        cached_cpu_idle_fraction(),
        read_loadavg_1m(),
        utilization_target,
        est_cores_per_sweep,
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
    // parse_proc_stat_cpu (Linux)
    // ------------------------------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_cpu_sums_total_and_idle_plus_iowait() {
        // cpu  user nice system idle iowait irq softirq steal guest guest_nice
        let s = "cpu  100 20 30 700 40 5 5 0 0 0\ncpu0 50 10 15 350 20 2 3 0 0 0\n";
        let sample = parse_proc_stat_cpu(s).unwrap();
        assert_eq!(sample.idle, 740, "idle = idle 700 + iowait 40");
        assert_eq!(sample.total, 100 + 20 + 30 + 700 + 40 + 5 + 5);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_cpu_skips_per_core_lines_and_rejects_malformed() {
        // A leading per-core line must not be mistaken for the aggregate.
        let s = "cpu0 1 2 3 4 5\ncpu 10 20 30 40 50\n";
        assert_eq!(parse_proc_stat_cpu(s).unwrap().idle, 90); // idle 40 + iowait 50
        assert_eq!(parse_proc_stat_cpu(""), None);
        assert_eq!(parse_proc_stat_cpu("intr 1 2 3\n"), None, "no aggregate cpu line");
        assert_eq!(parse_proc_stat_cpu("cpu 1 2\n"), None, "too few fields for idle+iowait");
        assert_eq!(parse_proc_stat_cpu("cpu a b c d e\n"), None, "non-numeric fields");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_idle_fraction_over_interval() {
        let prev = ProcStatCpu {
            idle: 700,
            total: 1000,
        };
        // Over the interval, total advanced 100 jiffies, idle 80 → 80% idle.
        let cur = ProcStatCpu {
            idle: 780,
            total: 1100,
        };
        let frac = cur.idle_fraction_since(&prev).unwrap();
        assert!((frac - 0.80).abs() < 1e-9, "got {frac}");
        // Zero total delta (or a reset counter) yields None, not a divide-by-zero.
        assert_eq!(cur.idle_fraction_since(&cur), None);
    }

    // ------------------------------------------------------------------
    // parse_iostat_cpu (macOS) — must use the SECOND (delta) data line
    // ------------------------------------------------------------------

    #[test]
    fn parse_iostat_cpu_uses_second_delta_line_not_first_since_boot() {
        // `iostat -c 2 -w 1 -n 0`: header, then a since-boot line (id 83) and a
        // 1-second delta line (id 50). Using the first line would report 0.83 —
        // this test FAILS if the parser reads the since-boot line.
        let out = "      cpu    load average\n \
                   us sy id   1m   5m   15m\n  \
                    8  9 83  3.37 4.02 4.64\n \
                   40 10 50  3.40 4.02 4.64\n";
        let idle = parse_iostat_cpu(out).unwrap();
        assert!((idle - 0.50).abs() < 1e-9, "must use the delta line (id 50), got {idle}");
    }

    #[test]
    fn parse_iostat_cpu_tolerates_leading_disk_columns() {
        // Without `-n 0`, disk columns precede the CPU columns. `id` is still the
        // fourth-from-last field, so parsing from the right is disk-count-agnostic.
        let out = "              disk0       cpu    load average\n    \
                   KB/t  tps  MB/s  us sy id   1m   5m   15m\n   \
                   13.45 4312 56.62   8  9 83  2.99 4.02 4.66\n   \
                   45.53  857 38.09   5  2 93  3.07 4.02 4.66\n";
        let idle = parse_iostat_cpu(out).unwrap();
        assert!((idle - 0.93).abs() < 1e-9, "got {idle}");
    }

    #[test]
    fn parse_iostat_cpu_malformed_is_none() {
        assert_eq!(parse_iostat_cpu(""), None);
        assert_eq!(
            parse_iostat_cpu("cpu load average\nus sy id 1m 5m 15m\n"),
            None,
            "no data lines"
        );
        // Only one data line (no delta line to use).
        assert_eq!(parse_iostat_cpu("us sy id 1m 5m 15m\n8 9 83 3.0 4.0 4.6\n"), None);
    }

    // ------------------------------------------------------------------
    // cpu_headroom — pure math (signature: ncpu, idle, loadavg, target, est)
    // ------------------------------------------------------------------

    #[test]
    fn cpu_headroom_static_capacity_when_no_signal_at_all() {
        // 8 cores, 75% target, no idle + no load reading -> capacity 6.0 / 2.0
        // cores per sweep = 3.
        assert_eq!(cpu_headroom(8, None, None, 0.75, 2.0), 3);
    }

    #[test]
    fn cpu_headroom_subtracts_current_load_when_no_idle_measurement() {
        // 8 cores, 75% target -> capacity 6.0. No idle measurement; load 4.0 in
        // use -> 2.0 available / 2.0 per sweep = 1 (the #3978 fallback path).
        assert_eq!(cpu_headroom(8, None, Some(4.0), 0.75, 2.0), 1);
    }

    #[test]
    fn cpu_headroom_prefers_measured_idle_over_loadavg() {
        // The #4031 core: 28 cores, 75% target -> capacity 21.0. 85% idle means
        // consumed = 28 × 0.15 = 4.2 cores; loadavg reads an inflated 6.0. The
        // term must reflect the idle reading (21 - 4.2 = 16.8 / 2.0 = 8), NOT the
        // load reading (21 - 6.0 = 15.0 / 2.0 = 7).
        assert_eq!(cpu_headroom(28, Some(0.85), Some(6.0), 0.75, 2.0), 8);
        // And it must ignore loadavg entirely when idle is present.
        assert_eq!(
            cpu_headroom(28, Some(0.85), None, 0.75, 2.0),
            cpu_headroom(28, Some(0.85), Some(999.0), 0.75, 2.0),
            "loadavg must not influence the term once a measured idle exists"
        );
    }

    #[test]
    fn cpu_headroom_floors_at_one_under_a_saturated_idle_reading() {
        // 0% idle on an 8-core/75% host -> consumed 8.0 > capacity 6.0 ->
        // available clamps to 0, but the term floors at 1 (soft backoff).
        assert_eq!(cpu_headroom(8, Some(0.0), None, 0.75, 2.0), 1);
        // Same floor via the loadavg path when load exceeds capacity.
        assert_eq!(cpu_headroom(8, None, Some(20.0), 0.75, 2.0), 1);
    }

    #[test]
    fn cpu_headroom_scales_with_more_cores() {
        // 32 cores, 75% target, no signal -> 24.0 / 2.0 = 12.
        assert_eq!(cpu_headroom(32, None, None, 0.75, 2.0), 12);
    }

    #[test]
    fn cpu_headroom_never_zero_even_on_a_single_core_host() {
        // 1 core, 75% target -> capacity 0.75, / 2.0 per sweep = 0.375 ->
        // floors to 0 mathematically, but the `max(1.0)` policy floor kicks in.
        assert_eq!(cpu_headroom(1, None, None, 0.75, 2.0), 1);
    }

    #[test]
    fn cpu_headroom_est_cores_per_sweep_scales_the_denominator() {
        // 16 cores, 100% target -> capacity 16.0. A heavier 4.0-cores-per-
        // sweep estimate -> 4.
        assert_eq!(cpu_headroom(16, None, None, 1.0, 4.0), 4);
    }

    #[test]
    fn cpu_headroom_idle_fraction_is_clamped() {
        // A nonsense idle fraction > 1 or < 0 can't push consumed negative /
        // above ncpu enough to break the term (defensive clamp).
        assert_eq!(cpu_headroom(8, Some(1.5), None, 0.75, 2.0), 3, "idle>1 clamps to full idle");
        assert_eq!(cpu_headroom(8, Some(-0.5), None, 0.75, 2.0), 1, "idle<0 clamps to fully busy");
    }

    // ------------------------------------------------------------------
    // env-only resolution (no config value in hand)
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
    // resolve_utilization_target / resolve_est_cores_per_sweep — env > config
    // > default precedence (#4032)
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn resolve_utilization_target_precedence() {
        std::env::remove_var(UTILIZATION_TARGET_ENV);

        // Both unset -> built-in default.
        assert!(
            (resolve_utilization_target(None) - DEFAULT_UTILIZATION_TARGET).abs() < f64::EPSILON
        );

        // Config set, env unset -> config wins.
        assert!((resolve_utilization_target(Some(0.6)) - 0.6).abs() < f64::EPSILON);

        // Env set, config set -> env wins.
        std::env::set_var(UTILIZATION_TARGET_ENV, "0.5");
        assert!((resolve_utilization_target(Some(0.6)) - 0.5).abs() < f64::EPSILON);

        // Env set, config unset -> env still wins.
        assert!((resolve_utilization_target(None) - 0.5).abs() < f64::EPSILON);

        std::env::remove_var(UTILIZATION_TARGET_ENV);
    }

    #[test]
    #[serial]
    fn resolve_est_cores_per_sweep_precedence() {
        std::env::remove_var(EST_CORES_PER_SWEEP_ENV);

        // Both unset -> built-in default.
        assert!(
            (resolve_est_cores_per_sweep(None) - DEFAULT_EST_CORES_PER_SWEEP).abs() < f64::EPSILON
        );

        // Config set, env unset -> config wins.
        assert!((resolve_est_cores_per_sweep(Some(4.0)) - 4.0).abs() < f64::EPSILON);

        // Env set, config set -> env wins.
        std::env::set_var(EST_CORES_PER_SWEEP_ENV, "1.0");
        assert!((resolve_est_cores_per_sweep(Some(4.0)) - 1.0).abs() < f64::EPSILON);

        // Env set, config unset -> env still wins.
        assert!((resolve_est_cores_per_sweep(None) - 1.0).abs() < f64::EPSILON);

        std::env::remove_var(EST_CORES_PER_SWEEP_ENV);
    }

    // Range validation of the config value (`(0, 1]` for utilization target,
    // `> 0` for cores-per-sweep) is applied at the config-read layer in
    // `read_work_finder_config` (see `work_finder.rs` tests
    // `test_config_out_of_range_cpu_utilization_target_drops_to_none` and
    // `test_config_out_of_range_est_cores_per_sweep_drops_to_none`), not here —
    // `resolve_utilization_target` / `resolve_est_cores_per_sweep` trust the
    // `Option<f64>` they are handed is already filtered, exactly like
    // `resolve_per_token_concurrency` trusts `WorkFinderConfig`.

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
        assert!(cpu_headroom_limit(DEFAULT_UTILIZATION_TARGET, DEFAULT_EST_CORES_PER_SWEEP) >= 1);
    }
}
