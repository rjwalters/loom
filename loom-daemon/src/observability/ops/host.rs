//! Host resource gauges (Issue #8860): memory, swap and worktree-volume
//! capacity, sampled on the `host.health` cadence and exported through
//! `metric.points`.
//!
//! CPU pressure is already exported from `host.health`
//! (`loom.host.cpu_idle_fraction`, `loom.host.load_per_core`). This adds the
//! byte-level readings a concurrency decision needs, following the same
//! "unknown != zero" rule as every other probe: an unmeasurable reading emits
//! no point, never a fabricated `0`.
//!
//! Sources, with no new crate dependency (the [`crate::ram_headroom`]
//! precedent): Linux reads `/proc/meminfo`; macOS uses `vm_stat` (free +
//! inactive pages, the `MemAvailable` analogue), `sysctl -n hw.memsize` and
//! `sysctl -n vm.swapusage`. The worktree volume is the same single `df -Pk`
//! sample `host.health` reads ([`crate::disk_headroom::worktree_root_disk_bytes`]),
//! handed in by the collector.

use crate::telemetry::ops::{MetricName, MetricPoint};

/// One host resource sample, in bytes. `None` = unmeasurable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostResources {
    pub memory_available: Option<u64>,
    pub memory_total: Option<u64>,
    pub swap_used: Option<u64>,
    pub swap_total: Option<u64>,
    pub worktree_volume_free: Option<u64>,
    pub worktree_volume_total: Option<u64>,
}

impl HostResources {
    /// One gauge per measured field.
    #[must_use]
    pub fn points(&self) -> Vec<MetricPoint> {
        [
            (MetricName::HostMemoryAvailableBytes, self.memory_available),
            (MetricName::HostMemoryTotalBytes, self.memory_total),
            (MetricName::HostSwapUsedBytes, self.swap_used),
            (MetricName::HostSwapTotalBytes, self.swap_total),
            (MetricName::HostWorktreeVolumeFreeBytes, self.worktree_volume_free),
            (MetricName::HostWorktreeVolumeTotalBytes, self.worktree_volume_total),
        ]
        .into_iter()
        .filter_map(|(name, value)| {
            Some(MetricPoint::int(name, i64::try_from(value?).unwrap_or(i64::MAX)))
        })
        .collect()
    }
}

/// Memory and swap from Linux `/proc/meminfo` (values in kB). Swap used is
/// `SwapTotal - SwapFree`.
#[must_use]
pub fn parse_meminfo(contents: &str) -> HostResources {
    let field = |label: &str| -> Option<u64> {
        let line = contents.lines().find(|l| l.starts_with(label))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb.saturating_mul(1024))
    };
    let swap_total = field("SwapTotal:");
    HostResources {
        memory_available: field("MemAvailable:"),
        memory_total: field("MemTotal:"),
        swap_used: swap_total
            .zip(field("SwapFree:"))
            .map(|(t, f)| t.saturating_sub(f)),
        swap_total,
        ..HostResources::default()
    }
}

/// Parse one `vm.swapusage` size such as `1024.50M` or `2.00G` into bytes.
fn parse_swap_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (number, unit) = raw.split_at(raw.find(|c: char| c.is_ascii_alphabetic())?);
    let multiplier = match unit {
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    // Truncation to whole bytes is intended; the input has two decimals.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((value * multiplier) as u64)
}

/// `(total, used)` swap bytes from macOS `sysctl -n vm.swapusage`, e.g.
/// `total = 2048.00M  used = 1024.50M  free = 1023.50M  (encrypted)`.
#[must_use]
pub fn parse_macos_swapusage(output: &str) -> Option<(u64, u64)> {
    let value = |label: &str| -> Option<u64> {
        let rest = &output[output.find(label)? + label.len()..];
        let rest = rest.trim_start().strip_prefix('=')?.trim_start();
        parse_swap_size(rest.split_whitespace().next()?)
    };
    Some((value("total")?, value("used")?))
}

#[cfg(target_os = "linux")]
fn sample_memory() -> HostResources {
    std::fs::read_to_string("/proc/meminfo")
        .map(|contents| parse_meminfo(&contents))
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn sysctl(name: &str) -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", name])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "macos")]
fn sample_memory() -> HostResources {
    let page_size: Option<u64> = sysctl("hw.pagesize").and_then(|v| v.parse().ok());
    let pages = std::process::Command::new("vm_stat")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            crate::ram_headroom::parse_vm_stat_available_pages(&String::from_utf8_lossy(&o.stdout))
        });
    let swap = sysctl("vm.swapusage").and_then(|v| parse_macos_swapusage(&v));
    HostResources {
        memory_available: pages.zip(page_size).map(|(p, s)| p.saturating_mul(s)),
        memory_total: sysctl("hw.memsize").and_then(|v| v.parse().ok()),
        swap_used: swap.map(|(_, used)| used),
        swap_total: swap.map(|(total, _)| total),
        ..HostResources::default()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample_memory() -> HostResources {
    HostResources::default()
}

/// Sample memory and swap, and combine them with the worktree-volume
/// `(free, total)` bytes the caller already probed. Blocking (subprocesses on
/// macOS): call from `spawn_blocking`.
///
/// The volume reading is passed in rather than probed here so one `df` sample
/// per `host.health` tick serves both `host.health`'s GB fields and these
/// byte gauges (#8857; it used to run `df` twice per tick).
#[must_use]
pub fn sample(worktree_volume: (Option<u64>, Option<u64>)) -> HostResources {
    let (worktree_volume_free, worktree_volume_total) = worktree_volume;
    HostResources {
        worktree_volume_free,
        worktree_volume_total,
        ..sample_memory()
    }
}

/// Sample and export, when an ops sink is registered. Async so the collector
/// can await it; the memory probes run on the blocking pool.
pub async fn record(worktree_volume: (Option<u64>, Option<u64>)) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    if let Ok(resources) = tokio::task::spawn_blocking(move || sample(worktree_volume)).await {
        sink.emit_metrics(resources.points());
    }
}
