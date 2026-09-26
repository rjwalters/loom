//! Host memory-pressure sampling for telemetry (the "what was the host doing
//! when the tick died" slice of the 2AMLogic trace gap).
//!
//! Loom's role spans have long exported CPU (headroom %, load-per-core) but no
//! memory signal, so a role attempt that ends in ~30 ms with no runtime child
//! span, or one that runs to the timeout ceiling on a saturated host, cannot be
//! told apart from *memory* pressure in the trace: was the attempt deferred for
//! host load, killed (OOM) by the kernel, or simply slow because the host was
//! swapping? This module closes that gap at the only boundary Loom can truth
//! tell — the host, at the moment the span begins or ends.
//!
//! # Measured, never fabricated
//!
//! Follows the exact "unknown != zero" contract `ram_headroom` /
//! `disk_headroom` / `cpu_headroom` established: every field is
//! `Option`; a probe that cannot measure its quantity leaves the field
//! `None` rather than coercing an unmeasured quantity to a fake
//! `0`. Cross-platform, the honest readings differ — that is deliberate:
//!
//! | field                      | Linux source              | macOS source                    |
//! | -------------------------- | ------------------------- | ------------------------------- |
//! | `mem_total_bytes`          | `/proc/meminfo` MemTotal  | `sysctl hw.memsize`             |
//! | `mem_available_bytes`      | `/proc/meminfo` MemAvailable | `vm_stat` (free + inactive) × page size |
//! | `mem_compressed_bytes`     | unmeasurable → `None`     | `vm_stat` "occupied by compressor" |
//! | `swap_total`/`swap_used`   | `/proc/meminfo` Swap*     | `sysctl vm.swapusage`           |
//! | swap in/out cumulative     | `/proc/vmstat` pswpin/out (512-byte units) | `vm_stat` Swapins/Swapouts (pages) |
//! | `memory_pressure`          | `/proc/pressure/memory` (PSI) | no PSI equivalent → `None` |
//! | `oom_kill_total`           | `/proc/vmstat` oom_kill   | no kernel counter → `None`      |
//!
//! In particular macOS exposes **no** kernel OOM-kill counter and **no** PSI
//! file; both stay `None` there instead of inventing a scale, and macOS
//! pressure shows through the compression and swap signals instead.
//!
//! # No new dependencies
//!
//! OS-native sources only (files under `/proc`, `sysctl`, `vm_stat`) plus the
//! existing `cpu_headroom` load reading — the same precedent as the sibling
//! headroom modules. Every parse is a pure function over the raw text so the
//! parsing is unit-tested against fixtures, and [`sample`] is the only
//! entry point that touches the OS.

use serde::{Deserialize, Serialize};

/// Memory-pressure classification, aligned with the Linux PSI vocabulary so a
/// fleet consumer sees one scale on every platform that can measure it.
///
/// PSI values are the fraction of wall time over `avg10` (10 s) during which
/// processes were delayed by the given condition: `some` = at least one
/// process made no forward progress, `full` = *no* process made forward
/// progress. We only carry the coarse three-state class, not the averages —
/// the averages are for tuning, the class is for the operator reading a
/// single span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryPressure {
    /// No stochastic memory stall observed (PSI read present, both averages at
    /// zero).
    None,
    /// Some process stalled non-blockingly on memory (PSI `some` > 0).
    Some,
    /// No process could make forward progress on memory (PSI `full` > 0) —
    /// the state an OOM kill is born in.
    Full,
}

impl MemoryPressure {
    /// Stable wire/span value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Some => "some",
            Self::Full => "full",
        }
    }

    /// Ordinal grade for gauges and dashboards: 0 = none, 1 = some, 2 = full.
    #[must_use]
    pub fn grade(self) -> u64 {
        match self {
            Self::None => 0,
            Self::Some => 1,
            Self::Full => 2,
        }
    }
}

/// One instantaneous, host-wide memory-pressure sample.
///
/// Every field is `Option`: the absence of a value is the *only* representation
/// of "the probe could not measure this on this platform". Consumers MUST treat
/// absent as unknown, never as zero — a `swap_used_bytes` of `None` on macOS
/// predating the `vm.swapusage` source must not render as "no swap in use".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostPressure {
    /// Total physical memory installed.
    pub mem_total_bytes: Option<u64>,
    /// Memory available to meet a new allocation without reclaim
    /// (`MemAvailable` on Linux; free + inactive pages on macOS).
    pub mem_available_bytes: Option<u64>,
    /// Memory the kernel has compressed rather than paged to swap
    /// (`vm_stat` compressor on macOS; unmeasurable on the Linux sources this
    /// reads, so `None` there).
    pub mem_compressed_bytes: Option<u64>,
    /// Total swap capacity, when the platform exposes one.
    pub swap_total_bytes: Option<u64>,
    /// Swap currently in use. Absent (not zero) where the platform exposes no
    /// counter — notably before a source existed to read it.
    pub swap_used_bytes: Option<u64>,
    /// Cumulative swap-in volume for the host's lifetime (monotonic; wraps
    /// only across a reboot). Used with [`swap_rates`] for a rate and diffed
    /// across a run's two boundary samples for "how much did this run swap".
    pub swap_in_bytes_total: Option<u64>,
    /// Cumulative swap-out volume for the host's lifetime.
    pub swap_out_bytes_total: Option<u64>,
    /// PSI class over the last 10 s, when the platform publishes PSI.
    pub memory_pressure: Option<MemoryPressure>,
    /// Cumulative kernel OOM kills (Linux `/proc/vmstat`). macOS has no
    /// equivalent counter and this stays `None` there.
    pub oom_kill_total: Option<u64>,
}

/// A previously observed cumulative swap-counter pair, timestamped, feeding
/// [`swap_rates`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwapCounterSample {
    /// When the counters were read.
    pub at: std::time::Instant,
    /// The cumulative swap-in byte total at `at`.
    pub swap_in_bytes_total: u64,
    /// The cumulative swap-out byte total at `at`.
    pub swap_out_bytes_total: u64,
}

/// Take a fresh, host-wide memory-pressure sample on the current platform.
///
/// Never panics on probe failure: every source degrades to `None`
/// independently. On Linux this is three small `/proc` reads; on macOS it is
/// one `vm_stat` plus two `sysctl` invocations — a few milliseconds, safe to
/// call on every span boundary and every `host.health` tick.
#[must_use]
pub fn sample() -> HostPressure {
    #[cfg(target_os = "macos")]
    {
        sample_macos()
    }
    #[cfg(target_os = "linux")]
    {
        sample_linux()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Unsupported platform: everything stays honestly absent.
        let _ = MemoryPressure::None;
        HostPressure::default()
    }
}

/// Per-second rates of the two cumulative swap counters between successive
/// samples.
///
/// Returns `(swap_in_bytes_per_sec, swap_out_bytes_per_sec)`. A per-counter
/// rate is `None` — never a fake `0.0` — when: there is no previous sample
/// (first call), a counter is missing on either side, the counter **rolled
/// back** (reboot or counter reset — a negative rate would be nonsense the
/// dashboard would plot as a dip), or the elapsed time is zero.
#[must_use]
pub fn swap_rates(
    prev: Option<SwapCounterSample>,
    now_in: Option<u64>,
    now_out: Option<u64>,
) -> (Option<f64>, Option<f64>) {
    let rate = |prev: u64, now: Option<u64>, at: std::time::Instant| {
        let now = now?;
        if now < prev {
            return None; // counter reset: the delta is not a rate
        }
        let secs = at.elapsed().as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        Some((now - prev) as f64 / secs)
    };
    match (prev, now_in, now_out) {
        (Some(prev), Some(in_now), Some(out_now)) => (
            rate(prev.swap_in_bytes_total, Some(in_now), prev.at),
            rate(prev.swap_out_bytes_total, Some(out_now), prev.at),
        ),
        _ => (None, None),
    }
}

/// True when a byte-rate `Option` carries a usable, finite, non-negative rate.
#[must_use]
pub fn valid_rate(r: f64) -> bool {
    r.is_finite() && r >= 0.0
}

#[cfg(target_os = "macos")]
fn sample_macos() -> HostPressure {
    let mut out = HostPressure::default();
    out.mem_total_bytes = sysctl_u64("hw.memsize");
    if let Ok(output) = std::process::Command::new("vm_stat").output() {
        if output.status.success() {
            let stats = parse_vm_stat_output(&String::from_utf8_lossy(&output.stdout));
            if let Some(page) = stats.page_size {
                if let (Some(free), Some(inactive)) = (stats.free_pages, stats.inactive_pages) {
                    out.mem_available_bytes =
                        Some(page.saturating_mul(free.saturating_add(inactive)));
                }
                if let Some(compressor) = stats.compressor_pages {
                    out.mem_compressed_bytes = Some(page.saturating_mul(compressor));
                }
                out.swap_in_bytes_total = stats.swapins.map(|n| page.saturating_mul(n));
                out.swap_out_bytes_total = stats.swapouts.map(|n| page.saturating_mul(n));
            }
        }
    }
    if let Ok(output) = std::process::Command::new("sysctl")
        .args(["-n", "vm.swapusage"])
        .output()
    {
        if output.status.success() {
            let (total, used) = parse_swapusage(&String::from_utf8_lossy(&output.stdout));
            out.swap_total_bytes = total;
            out.swap_used_bytes = used;
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn sample_linux() -> HostPressure {
    let mut out = HostPressure::default();
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        let m = parse_meminfo(&text);
        out.mem_total_bytes = m.mem_total_kb.map(|kb| kb.saturating_mul(1024));
        out.mem_available_bytes = m.mem_available_kb.map(|kb| kb.saturating_mul(1024));
        match (m.swap_total_kb, m.swap_free_kb) {
            (Some(total), Some(free)) if free <= total => {
                out.swap_total_bytes = Some(total.saturating_mul(1024));
                out.swap_used_bytes = Some(total.saturating_sub(free).saturating_mul(1024));
            }
            (Some(total), _) => {
                // SwapFree unreadable: the total is a fact, the usage is not
                // — keep only the measured half.
                out.swap_total_bytes = Some(total.saturating_mul(1024));
            }
            _ => {}
        }
    }
    if let Ok(text) = std::fs::read_to_string("/proc/pressure/memory") {
        out.memory_pressure = parse_psi_memory(&text);
    }
    if let Ok(text) = std::fs::read_to_string("/proc/vmstat") {
        let v = parse_vmstat(&text);
        // pswpin/pswpout are 512-byte units, not page units — normalize to
        // bytes so the fleet-wide field has one meaning on every platform.
        out.swap_in_bytes_total = v.pswpin.map(|n| n.saturating_mul(512));
        out.swap_out_bytes_total = v.pswpout.map(|n| n.saturating_mul(512));
        out.oom_kill_total = v.oom_kill;
    }
    out
}

// ---------------------------------------------------------------------------
// Pure parsers (fixture-tested)
// ---------------------------------------------------------------------------

/// `/proc/meminfo` values in kB, one `Option` per key read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeminfoSample {
    /// `MemTotal` (kB).
    pub mem_total_kb: Option<u64>,
    /// `MemAvailable` (kB); absent on kernels that predate it.
    pub mem_available_kb: Option<u64>,
    /// `SwapTotal` (kB).
    pub swap_total_kb: Option<u64>,
    /// `SwapFree` (kB).
    pub swap_free_kb: Option<u64>,
}

/// Parse a `/proc/meminfo` snapshot. Lines are `Key: <value> kB`; the unit is
/// always kB on Linux, and a missing key leaves its field `None`.
#[must_use]
pub fn parse_meminfo(text: &str) -> MeminfoSample {
    let mut out = MeminfoSample::default();
    for line in text.lines() {
        let (key, rest) = match line.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        let value = rest.split_whitespace().next().and_then(|v| v.parse().ok());
        match key.trim() {
            "MemTotal" => out.mem_total_kb = value,
            "MemAvailable" => out.mem_available_kb = value,
            "SwapTotal" => out.swap_total_kb = value,
            "SwapFree" => out.swap_free_kb = value,
            _ => {}
        }
    }
    out
}

/// Parse a `/proc/pressure/memory` PSI snapshot.
///
/// ```text
/// some avg10=0.00 avg60=0.00 avg300=0.00 total=1234567
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=0
/// ```
///
/// The 10-second average is the window a role attempt lives in. `None` when no
/// recognizable line is present (the file may be absent or unreadable on some
/// kernels/containers); `MemoryPressure::None` when PSI is readable and relaxed.
#[must_use]
pub fn parse_psi_memory(text: &str) -> Option<MemoryPressure> {
    let mut some: Option<f64> = None;
    let mut full: Option<f64> = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next()?;
        let avg10 = fields
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse().ok());
        match kind {
            "some" => some = avg10,
            "full" => full = avg10,
            _ => {}
        }
    }
    match (some, full) {
        (Some(full10), Some(_)) if full10 > 0.0 => Some(MemoryPressure::Full),
        (Some(some10), _) if some10 > 0.0 => Some(MemoryPressure::Some),
        (Some(0.0), _) => Some(MemoryPressure::None),
        _ => None,
    }
}

/// `/proc/vmstat` counters relevant to memory pressure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmstatSample {
    /// `pswpin` — pages swapped in, in 512-byte units on Linux.
    pub pswpin: Option<u64>,
    /// `pswpout` — pages swapped out, in 512-byte units on Linux.
    pub pswpout: Option<u64>,
    /// `oom_kill` — kernel OOM kills since boot, when the kernel exposes it.
    pub oom_kill: Option<u64>,
}

/// Parse a `/proc/vmstat` snapshot. Unlisted keys stay `None` — notably an
/// older kernel with no `oom_kill` line must not read as "zero OOM kills".
#[must_use]
pub fn parse_vmstat(text: &str) -> VmstatSample {
    let mut out = VmstatSample::default();
    for line in text.lines() {
        let (key, value) = match line.split_once(' ') {
            Some(v) => v,
            None => continue,
        };
        let value = value.trim().parse().ok();
        match key {
            "pswpin" => out.pswpin = value,
            "pswpout" => out.pswpout = value,
            "oom_kill" => out.oom_kill = value,
            _ => {}
        }
    }
    out
}

/// macOS `vm_stat` output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmStatSample {
    /// Page size from the `Mach Virtual Memory Statistics: (page size of N
    /// bytes)` header. `None` when the header is missing — every count below
    /// is then unusable, so callers must gate on this.
    pub page_size: Option<u64>,
    /// `Pages free`.
    pub free_pages: Option<u64>,
    /// `Pages inactive` (reclaimable without faulting the process out).
    pub inactive_pages: Option<u64>,
    /// `Pages occupied by compressor`.
    pub compressor_pages: Option<u64>,
    /// `Swapins` — cumulative swap-in page transfers.
    pub swapins: Option<u64>,
    /// `Swapouts` — cumulative swap-out page transfers.
    pub swapouts: Option<u64>,
}

/// Parse macOS `vm_stat` output.
///
/// ```text
/// Mach Virtual Memory Statistics: (page size of 16384 bytes)
///
///   Pages free:                                11234.
///   Pages active:                             234567.
///   Pages inactive:                           223456.
///   ...
///   Pages occupied by compressor:               98765.
///   ...
///   Swapins:                                    1234.
///   Swapouts:                                    567.
/// ```
///
/// Values are page counts with a trailing `.`; the page size is machine- and
/// architecture-dependent (16384 on Apple silicon) so it comes from the header,
/// never a constant.
#[must_use]
pub fn parse_vm_stat_output(text: &str) -> VmStatSample {
    let mut out = VmStatSample::default();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("Mach Virtual Memory Statistics:") {
            if let Some(start) = header.find("page size of ") {
                let rest = &header[start + "page size of ".len()..];
                if let Some(end) = rest.find(' ') {
                    out.page_size = rest[..end].parse().ok();
                }
            }
            continue;
        }
        let (key, value) = match line.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        let value = value.trim().trim_end_matches('.').parse().ok();
        match key.trim() {
            "Pages free" => out.free_pages = value,
            "Pages inactive" => out.inactive_pages = value,
            "Pages occupied by compressor" => out.compressor_pages = value,
            "Swapins" => out.swapins = value,
            "Swapouts" => out.swapouts = value,
            _ => {}
        }
    }
    out
}

/// Parse `sysctl vm.swapusage` output:
/// `total = 29696.00M  used = 28282.19M  free = 1413.81M  (encrypted)`.
///
/// Returns `(total_bytes, used_bytes)`; either half `None` if its key or unit
/// fails to parse. macOS reports swap in decimal megabytes; the `M` unit is
/// 1024 × 1024 bytes.
#[must_use]
pub fn parse_swapusage(text: &str) -> (Option<u64>, Option<u64>) {
    let value_for = |key: &str| -> Option<u64> {
        // The line is a flat string; find `key = <number><unit>` by scanning
        // whitespace-separated tokens for the key's `=` token.
        let tokens: Vec<&str> = text.split_whitespace().collect();
        let idx = tokens.iter().position(|t| *t == key)?;
        let eq = *tokens.get(idx.checked_add(1)?)?;
        if eq != "=" {
            return None;
        }
        let num_and_unit = *tokens.get(idx.checked_add(2)?)?;
        let (num, unit) = num_and_unit.split_once(|c: char| !c.is_ascii_digit() && c != '.')?;
        let num: f64 = num.parse().ok()?;
        let mult: f64 = match unit {
            "M" => 1024.0 * 1024.0,
            "KB" => 1024.0,
            "B" => 1.0,
            _ => return None,
        };
        Some((num * mult).round() as u64)
    };
    (value_for("total"), value_for("used"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MEMINFO: &str = "\
MemTotal:             36734096 kB
MemFree:               8123456 kB
MemAvailable:         21431040 kB
Buffers:                123456 kB
Cached:                9876543 kB
SwapTotal:            10485760 kB
SwapFree:              9502720 kB
";

    #[test]
    fn meminfo_parses_all_known_keys() {
        let m = parse_meminfo(MEMINFO);
        assert_eq!(m.mem_total_kb, Some(36_734_096));
        assert_eq!(m.mem_available_kb, Some(21_431_040));
        assert_eq!(m.swap_total_kb, Some(10_485_760));
        assert_eq!(m.swap_free_kb, Some(9_502_720));
    }

    #[test]
    fn meminfo_missing_key_stays_none() {
        let m = parse_meminfo("MemTotal: 8192000 kB\n");
        assert_eq!(m.mem_total_kb, Some(8_192_000));
        assert_eq!(m.mem_available_kb, None); // pre-MemAvailable kernel
        assert_eq!(m.swap_total_kb, None);
        assert_eq!(m.swap_free_kb, None);
    }

    #[test]
    fn meminfo_garbage_never_panics() {
        assert_eq!(parse_meminfo(""), MeminfoSample::default());
        let m = parse_meminfo("\n  \tMemTotal:notanumber kB\n");
        assert_eq!(m.mem_total_kb, None);
    }

    const PSI_RELAXED: &str = "\
some avg10=0.00 avg60=0.00 avg300=0.00 total=1234567
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
";

    #[test]
    fn psi_maps_the_three_states() {
        assert_eq!(parse_psi_memory(PSI_RELAXED), Some(MemoryPressure::None));
        assert_eq!(
            parse_psi_memory(
                "some avg10=2.31 avg60=1.10 avg300=0.42 total=999\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
            ),
            Some(MemoryPressure::Some)
        );
        assert_eq!(
            parse_psi_memory(
                "some avg10=55.00 avg60=30.00 avg300=12.00 total=5000\nfull avg10=12.50 avg60=5.00 avg300=2.00 total=409\n"
            ),
            Some(MemoryPressure::Full)
        );
        // A `full` reading outranks `some` even when both are nonzero.
        assert_eq!(
            parse_psi_memory(
                "some avg10=5.00 avg60=1.00 avg300=0.50 total=1\nfull avg10=0.01 avg60=0.00 avg300=0.00 total=2\n"
            ),
            Some(MemoryPressure::Full)
        );
    }

    #[test]
    fn psi_unreadable_is_none_not_relaxed() {
        // "file exists but none of our lines" and empty input are NOT proof
        // the host is relaxed — they are an unknown.
        assert_eq!(parse_psi_memory(""), None);
        assert_eq!(parse_psi_memory("bogus line"), None);
    }

    #[test]
    fn vmstat_counters_and_absence() {
        let v = parse_vmstat("pswpin 123456\npswpout 43210\noom_kill 7\npgpgin 9\n");
        assert_eq!(v.pswpin, Some(123456));
        assert_eq!(v.pswpout, Some(43210));
        assert_eq!(v.oom_kill, Some(7));

        let old = parse_vmstat("pswpin 1\npswpout 2\n");
        assert_eq!(old.oom_kill, None); // older kernel: not zero, unknown
    }

    const VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)

Pages free:                                11234.
Pages active:                            234567.
Pages inactive:                          223456.
Pages speculative:                          123.
Pages occupied by compressor:               98765.
Decompressions:                          987654.
Compressions:                            876543.
Pageouts:                                 12345.
Swapins:                                   1234.
Swapouts:                                   567.
";

    #[test]
    fn vm_stat_parses_page_size_and_counts() {
        let v = parse_vm_stat_output(VM_STAT);
        assert_eq!(v.page_size, Some(16384));
        assert_eq!(v.free_pages, Some(11234));
        assert_eq!(v.inactive_pages, Some(223456));
        assert_eq!(v.compressor_pages, Some(98765));
        assert_eq!(v.swapins, Some(1234));
        assert_eq!(v.swapouts, Some(567));
    }

    #[test]
    fn vm_stat_missing_header_leaves_page_size_none() {
        let v = parse_vm_stat_output("Pages free: 100.\nSwapins: 1.\n");
        assert_eq!(v.page_size, None);
        assert_eq!(v.free_pages, Some(100));
        assert_eq!(v.swapins, Some(1));
    }

    #[test]
    fn vm_stat_wrong_page_size_is_not_assumed() {
        // Intel Macs historically ran 4096-byte pages; the parser must take
        // whatever the header says.
        let v = parse_vm_stat_output(
            "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free: 2.\n",
        );
        assert_eq!(v.page_size, Some(4096));
        assert_eq!(v.free_pages, Some(2));
    }

    #[test]
    fn swapusage_parses_megabytes() {
        let (total, used) =
            parse_swapusage("total = 29696.00M  used = 28282.19M  free = 1413.81M  (encrypted)");
        assert_eq!(total, Some((29696.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(used, Some((28282.19_f64 * 1024.0 * 1024.0).round() as u64));
    }

    #[test]
    fn swapusage_garbage_is_honestly_none() {
        assert_eq!(parse_swapusage("sysctl: unknown oid 'vm.swapusage'"), (None, None));
        assert_eq!(parse_swapusage(""), (None, None));
        assert_eq!(parse_swapusage("total = 12M"), (Some(12 << 20), None));
    }

    fn sample_at(in_t: u64, out_t: u64, secs_ago: u64) -> SwapCounterSample {
        SwapCounterSample {
            at: std::time::Instant::now() - Duration::from_secs(secs_ago),
            swap_in_bytes_total: in_t,
            swap_out_bytes_total: out_t,
        }
    }

    #[test]
    fn rates_first_sample_is_none() {
        assert_eq!(swap_rates(None, Some(100), Some(200)), (None, None));
    }

    #[test]
    fn rates_compute_per_second() {
        // 1 MiB in over ~10 s ≈ 104857.6 B/s; 512 B out ≈ 51.2 B/s (float slack).
        let (rin, rout) = swap_rates(Some(sample_at(0, 0, 10)), Some(1_048_576), Some(512));
        assert!(rin.unwrap() > 100_000.0 && rin.unwrap() < 110_000.0);
        assert!(rout.unwrap() > 40.0 && rout.unwrap() < 60.0);
    }

    #[test]
    fn rates_counter_rollback_is_none() {
        // A reboot drops the counters to zero: the "delta" is negative and
        // must read as unknown, never as a negative rate.
        let (rin, rout) =
            swap_rates(Some(sample_at(9_999_999, 9_999_999, 30)), Some(100), Some(50));
        assert_eq!(rin, None);
        assert_eq!(rout, None);
    }

    #[test]
    fn rates_missing_counter_is_none_per_side() {
        let (rin, rout) = swap_rates(Some(sample_at(0, 0, 5)), None, Some(10));
        assert_eq!(rin, None);
        assert_eq!(rout, Some(2.0));
    }

    #[test]
    fn rates_zero_elapsed_is_none() {
        let (rin, rout) = swap_rates(Some(sample_at(0, 0, 0)), Some(10), Some(10));
        assert_eq!(rin, None);
        assert_eq!(rout, None);
    }

    #[test]
    fn sample_never_panics_and_reads_something_on_supported_platforms() {
        let p = sample();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let any_measured = [
                p.mem_total_bytes.is_some(),
                p.mem_available_bytes.is_some(),
                p.swap_total_bytes.is_some(),
                p.oom_kill_total.is_some(),
            ]
            .into_iter()
            .any(|x| x);
            assert!(
                any_measured,
                "host_pressure::sample() measured nothing on a supported platform: {p:?}"
            );
        }
        #[cfg(target_os = "macos")]
        {
            assert!(p.mem_total_bytes.is_some(), "hw.memsize should read");
        }
    }

    #[test]
    fn pressure_wire_values_are_stable() {
        assert_eq!(MemoryPressure::None.as_str(), "none");
        assert_eq!(MemoryPressure::Some.as_str(), "some");
        assert_eq!(MemoryPressure::Full.as_str(), "full");
        assert_eq!(
            (
                MemoryPressure::None.grade(),
                MemoryPressure::Some.grade(),
                MemoryPressure::Full.grade()
            ),
            (0, 1, 2)
        );
        let json = serde_json::to_string(&MemoryPressure::Full).unwrap();
        assert_eq!(json, "\"full\"");
    }
}
