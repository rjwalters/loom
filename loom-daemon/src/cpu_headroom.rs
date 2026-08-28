//! Host CPU **measurement** — logical core count, load average, and measured
//! idle fraction (#3978/#4031), plus the shared "is the host saturated?"
//! predicate (#4259).
//!
//! # This module no longer gates admission (#4512)
//!
//! Until #4512 this module also computed a *concurrency term*: `cpu_headroom =
//! (logical_cpus × cpuUtilizationTarget − consumed_cores) / estCoresPerSweep`,
//! folded into [`crate::work_finder::resolve_dynamic_max_concurrent`]'s
//! `min(...)`. That term is **deleted**. It priced every sweep as if it were a
//! build (`estCoresPerSweep`), so it throttled the ~90% of sweep wall-clock
//! that is API-wait (curator / builder / judge conversations) in order to
//! defend against the minority heavy-build case — measurably: an 8-core worker
//! sitting **95% idle** was capped at 2 concurrent sweeps. The replacement, per
//! #4512, is two-part:
//!
//! - **admission** is one per-machine knob (`autonomous.workFinder.maxConcurrent`),
//!   tuned empirically by the operator, alongside the two hard exhaustible-resource
//!   floors (the token axis and disk headroom);
//! - **the genuinely heavy stages serialize where they occur**, via the
//!   machine-wide build slot ([`crate::build_slot`]).
//!
//! What remains here is pure measurement, consumed by:
//!
//! - the **host-distress circuit breaker** (#4235 — [`load_per_core`]), the
//!   load safety net that makes a hand-tuned ceiling safe to raise: a mis-set
//!   knob trips a *measured* breaker instead of melting the host;
//! - the **build gate's load-aware deferral** (#4259 — [`is_host_saturated`]);
//! - `loom-daemon status` / `loom-daemon calibrate`, which report the host's
//!   observed idle fraction so an operator can *see* whether the current
//!   `maxConcurrent` is leaving the host idle or saturated.
//!
//! `LOOM_CPU_UTILIZATION_TARGET` / `LOOM_EST_CORES_PER_SWEEP` (and their
//! `autonomous.*` config twins) are **accepted-but-ignored** with a one-shot
//! deprecation warning — see [`crate::work_finder::warn_deprecated_cpu_knobs`].
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
//! CPU sat idle. So the reported CPU **consumption** signal is a measured idle
//! fraction, per-platform, with the load average kept only as a fallback for
//! reporting and as the (separate, deliberately load-average-based) input to
//! the saturation predicate and the host breaker.
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
//! Every read here returns `Option` and **absent evidence is never treated as
//! bad news**: an unreadable idle signal (no `/proc/stat`, missing `iostat`,
//! unsupported platform, or simply no delta sampled yet) is `None`, and every
//! consumer must fail safe on `None` rather than assume "host fully loaded" —
//! [`is_host_saturated`] returns `None` so the build gate runs instead of
//! deferring, and the host breaker treats a missing sample as a non-observation.
//! This mirrors [`crate::capacity::token_axis_limit`]'s policy of falling back
//! to the optimistic basis (the raw pool) when no ranking data exists: the
//! absence of a signal is not evidence of unhealthiness.

use std::sync::Mutex;
use std::time::{Duration, Instant};

// `Command`/`Stdio` back only the macOS `sysctl` / `iostat` read paths —
// cfg-gated so a Linux build (which reads `/proc/*` directly, no subprocess)
// doesn't warn on an unused import under `-D warnings`.
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

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

/// Pure: load-per-core = `loadavg_1m / ncpu`, or `None` when the load average
/// is unavailable or the CPU count is zero. The host-distress circuit breaker
/// (#4235) trips on this normalized ratio so its threshold is portable across
/// hosts of different core counts (a raw load of 100 is catastrophic on 8 cores
/// but routine on 128).
#[must_use]
pub fn load_per_core_from(loadavg_1m: Option<f64>, ncpu: usize) -> Option<f64> {
    let load = loadavg_1m?;
    if ncpu == 0 {
        return None;
    }
    Some(load / ncpu as f64)
}

/// Sample the current load-per-core from live host inputs for the host-distress
/// circuit breaker (#4235). Uses the **fast**, non-sleeping load-average read
/// ([`read_loadavg_1m`] — a `/proc/loadavg` read on Linux, a quick `sysctl` on
/// macOS), never the ~1s `iostat` idle sample, so it is safe to call inline from
/// the work-finder loop each tick. `None` when no load-average source exists.
#[must_use]
pub fn load_per_core() -> Option<f64> {
    load_per_core_from(read_loadavg_1m(), logical_cpu_count())
}

// ============================================================================
// Host saturation — shared "is the host loaded?" predicate (#4259)
// ============================================================================
//
// The build-gate's load-aware deferral (`main_health_gate::run_gate_tick`) and
// any future load-aware dispatch agree on one definition of "saturated" by
// funneling through these pure helpers, rather than each re-deriving a ratio.
// Deliberately load-average-based (not the measured idle fraction): the gate
// deferral must work from the very first tick, before the idle-fraction memo
// ([`refresh_cpu_util_cache`]) has been warmed by a work-finder loop, and a
// missing load reading is a hard fail-safe signal (run the gate, never defer).

/// Default 1-minute-load-average-per-logical-CPU ratio at or above which the
/// host is considered *saturated* for build-gate deferral (#4259). A ratio of
/// `1.0` means "as many runnable/uninterruptible threads as logical cores"; the
/// gate defers a notch below that (`0.9`) so it does not pile its own multi-core
/// `cargo` build onto an already-full host. Tunable via config/env — the load
/// average overstates consumption on macOS (see the module note on #4031), so an
/// operator may raise it.
///
/// This is deliberately distinct from — and well below — the host-distress
/// circuit breaker's [`crate::host_breaker::DEFAULT_HOST_BREAKER_LOAD_PER_CORE`]
/// (`2.5`): the two thresholds measure the **same** normalized ratio
/// ([`load_per_core_from`]) but encode different policies. `0.9` is "the host is
/// full enough that the gate should wait a cycle rather than add its own build",
/// a low-cost *scheduling* deferral; `2.5` is "the host is in genuine distress,
/// stop dispatching new sweeps entirely", a protective *trip*. The gate defer
/// point sits below the breaker trip on purpose so the gate backs off long
/// before the host reaches distress — this ordering is intentional, not drift.
pub const DEFAULT_GATE_LOAD_THRESHOLD: f64 = 0.9;

/// Whether the host is saturated at `threshold`, given an optional load reading
/// (#4259). Pure — the decision function. Built on the shared
/// [`load_per_core_from`] ratio (#4316) so the gate and the host-distress
/// circuit breaker agree on one definition of load-per-core. `None` load (or a
/// zero CPU count) ⇒ `None` here: the caller must fail safe (run the gate, never
/// defer) on absent evidence, never treat a missing reading as "loaded".
///
/// The gate (`main_health_gate::run_gate_tick`) samples [`read_loadavg_1m`] /
/// [`logical_cpu_count`] once per tick and passes them here, reusing the same
/// reading for the ratio it logs, so there is no separate live-probe wrapper.
#[must_use]
pub fn is_host_saturated(loadavg_1m: Option<f64>, ncpu: usize, threshold: f64) -> Option<bool> {
    load_per_core_from(loadavg_1m, ncpu).map(|lpc| lpc >= threshold)
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

/// Take a single, un-memoized `/proc/stat` snapshot for a caller that wants
/// to **bracket its own short operation window** — call once before the
/// operation and once after, then [`ProcStatCpu::idle_fraction_since`] gives
/// the CPU-busy fraction over exactly that window (#7025).
///
/// This is deliberately *not* [`refresh_cpu_util_cache`]'s process-global
/// memoized delta (that state is shared across the whole process and TTL'd
/// to 10s — useless for bracketing a single multi-second test) nor
/// [`load_per_core`] (a 1-minute exponentially-decaying average that, per
/// #7025's measurement, can under-report a multi-second full-core-saturation
/// burst by an order of magnitude: driving all cores to 100% for ~4s on an
/// idle 8-core host moved the reported 1-minute load-per-core by only a few
/// hundredths — nowhere near the `> 1.0` widening threshold). No sleep, no
/// shared mutable state — cheap enough to call inline from an async test.
///
/// Linux-only: non-Linux platforms have no cheap non-sleeping CPU-delta
/// source (macOS's only option, `iostat`, blocks for ~1s — unusable for
/// bracketing a test inline). Callers on other platforms must fall back to
/// [`load_per_core`] alone, exactly like every other `Option`-returning
/// reader in this module (see the module doc's "Fail-open, not fail-closed").
#[cfg(target_os = "linux")]
#[must_use]
pub fn sample_proc_stat_cpu() -> Option<ProcStatCpu> {
    read_proc_stat_cpu()
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
    // host saturation (#4259) — pure predicate
    // ------------------------------------------------------------------

    #[test]
    fn is_host_saturated_thresholds_on_load_per_core() {
        // 28-core host, threshold 0.9: load 30 (1.07/core) is saturated.
        assert_eq!(is_host_saturated(Some(30.0), 28, 0.9), Some(true));
        // load 14 (0.5/core) is not.
        assert_eq!(is_host_saturated(Some(14.0), 28, 0.9), Some(false));
        // Exactly at the threshold counts as saturated (>=).
        assert_eq!(is_host_saturated(Some(25.2), 28, 0.9), Some(true));
    }

    #[test]
    fn is_host_saturated_missing_load_is_none_fail_safe() {
        // No load reading ⇒ None (the caller runs the gate, never defers).
        assert_eq!(is_host_saturated(None, 28, 0.9), None);
        // A zero CPU count is likewise absent evidence (shared
        // `load_per_core_from` guard) ⇒ None, so the caller still fails safe.
        assert_eq!(is_host_saturated(Some(3.0), 0, 0.9), None);
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
    // refresh + cached idle fraction — smoke test against the real host
    // ------------------------------------------------------------------

    #[test]
    fn refresh_cpu_util_cache_never_panics_and_the_cache_is_readable() {
        // Purely observational now (#4512): the memo feeds `loom-daemon status`
        // / `calibrate` reporting, not any admission decision. Whatever this
        // host supports, neither call may panic and a read must be well-typed.
        refresh_cpu_util_cache();
        let frac = cached_cpu_idle_fraction();
        if let Some(f) = frac {
            assert!((0.0..=1.0).contains(&f), "idle fraction out of range: {f}");
        }
    }
}
