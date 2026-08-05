//! RAM-headroom math for the autonomous work finder (#5270 — "dumb mode").
//!
//! # Why this exists
//!
//! #5270 removed the token axis from the dynamic concurrency cap entirely
//! (operator direction: "we should only ever limit parallelism based on the
//! machine disk/RAM/CPU" — a metered API key or an overage-enabled
//! subscription pool has no exhaustible per-account ceiling worth modeling).
//! Disk headroom ([`crate::disk_headroom`]) and the CPU saturation admission
//! brake ([`crate::admission_brake`]) already covered two of those three
//! machine axes; this module is the third — the available-RAM analog of
//! [`crate::disk_headroom`], mirroring its shape exactly:
//!
//! - [`available_ram_gb`] reads the host's currently-available memory (not
//!   total — "available" already accounts for the kernel's own reclaimable
//!   caches/buffers, so it is the number a scheduler should compare against,
//!   the same distinction `free -h`'s `available` column makes over `free`).
//!   `None` means the probe was unmeasurable, not that 0 GB is available
//!   (mirrors disk headroom's #4164 "unknown != zero" policy).
//! - [`ram_headroom`] is the pure floor-division term: how many worktrees
//!   `available_gb` can host at `LOOM_PER_WORKTREE_RAM_GB` each.
//! - [`ram_headroom_limit`] combines the two, and — exactly like
//!   [`crate::disk_headroom::disk_headroom_limit`] — SKIPS the clamp
//!   (`usize::MAX`) when the probe is unmeasurable, so an unsupported
//!   platform or a missing `/proc/meminfo` never silently pins concurrency
//!   to 0.
//!
//! # Why read `/proc/meminfo` / shell to `vm_stat` instead of a `sysinfo`-style
//! # crate
//!
//! Same precedent as [`crate::disk_headroom`] (shell to `df`) and
//! [`crate::cpu_headroom`] (read `/proc/stat` / shell to `iostat`): no new
//! crate dependency, OS-native sources kept small and split into pure parsing
//! functions ([`parse_meminfo_available_kb`], [`parse_vm_stat_available_pages`])
//! so the arithmetic is unit-testable without a real host.
//!
//! - **Linux** reads `MemAvailable` directly from `/proc/meminfo` — the
//!   kernel's own estimate of memory available for new allocations without
//!   swapping (accounts for reclaimable slab/page-cache; a raw `MemFree` would
//!   undercount by treating reclaimable cache as unavailable).
//! - **macOS** has no `MemAvailable` equivalent; `vm_stat` reports free +
//!   inactive pages (inactive pages are reclaimable, mirroring what
//!   `MemAvailable` counts on Linux) and `sysctl -n hw.pagesize` gives the
//!   page size to convert to bytes.
//!
//! # Fail-open, not fail-closed
//!
//! Mirrors [`crate::disk_headroom`]: an unmeasurable probe returns `None` /
//! `usize::MAX` (skip the clamp), never a fabricated `Some(0)` / `0` that would
//! look identical to "genuinely out of RAM".

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

/// Environment variable overriding the conservative per-worktree RAM estimate
/// (GB). Mirrors [`crate::disk_headroom::PER_WORKTREE_GB_ENV`].
pub const PER_WORKTREE_RAM_GB_ENV: &str = "LOOM_PER_WORKTREE_RAM_GB";

/// Default per-worktree RAM estimate (GB). A `claude`-driven sweep's own
/// footprint is modest (client process + git + occasional `cargo`), so this
/// mirrors the disk default rather than assuming a build-heavy workload —
/// same posture disk headroom takes, and just as tunable per host.
pub const DEFAULT_PER_WORKTREE_RAM_GB: u64 = 2;

/// Resolve the per-worktree RAM GB estimate from [`PER_WORKTREE_RAM_GB_ENV`],
/// flooring to a minimum of 1 (mirrors [`crate::disk_headroom::per_worktree_gb`]).
#[must_use]
pub fn per_worktree_ram_gb() -> u64 {
    std::env::var(PER_WORKTREE_RAM_GB_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_PER_WORKTREE_RAM_GB)
}

/// Parse `MemAvailable` (in kB) from Linux `/proc/meminfo` contents.
///
/// `/proc/meminfo` lines are `"<Key>:<spaces><value> kB\n"`. `MemAvailable` is
/// the kernel's own estimate of memory available for new allocations without
/// swapping — it already accounts for reclaimable slab/page-cache, unlike
/// `MemFree` (which would undercount). Returns `None` when the field is
/// missing or malformed.
#[must_use]
pub fn parse_meminfo_available_kb(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|l| l.starts_with("MemAvailable:"))?;
    // "MemAvailable:    1234567 kB" — the numeric field is the 2nd token.
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Read the current `MemAvailable` (GB) on Linux via `/proc/meminfo`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_available_kb(&contents).map(|kb| kb / 1024 / 1024)
}

/// Parse the free + inactive page counts from macOS `vm_stat` output.
///
/// `vm_stat` prints one `"<Label>:<spaces><count>."` line per stat; the
/// reclaimable-equivalent of Linux's `MemAvailable` is free + inactive pages
/// (inactive pages hold evictable content, mirroring what `MemAvailable`
/// counts). Returns `None` when either field is missing/malformed.
#[must_use]
pub fn parse_vm_stat_available_pages(output: &str) -> Option<u64> {
    let field = |label: &str| -> Option<u64> {
        let line = output.lines().find(|l| l.trim_start().starts_with(label))?;
        let value = line.rsplit(':').next()?.trim().trim_end_matches('.');
        value.parse().ok()
    };
    let free = field("Pages free")?;
    let inactive = field("Pages inactive")?;
    Some(free + inactive)
}

/// Read the host's page size (bytes) via `sysctl -n hw.pagesize` on macOS.
#[cfg(target_os = "macos")]
fn read_macos_page_size() -> Option<u64> {
    let output = Command::new("sysctl")
        .arg("-n")
        .arg("hw.pagesize")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Read the current available RAM (GB) on macOS via `vm_stat` (free + inactive
/// pages) × the host page size.
#[cfg(target_os = "macos")]
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    let output = Command::new("vm_stat")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pages = parse_vm_stat_available_pages(&String::from_utf8_lossy(&output.stdout))?;
    let page_size = read_macos_page_size()?;
    Some(pages.saturating_mul(page_size) / 1024 / 1024 / 1024)
}

/// No known available-RAM source on other platforms — the caller skips the
/// RAM clamp entirely (mirrors [`crate::cpu_headroom::read_loadavg_1m`]'s
/// "no source" fallback).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    None
}

/// The RAM-headroom concurrency term: how many worktrees `available_gb` can
/// host at `per_gb` GB each. Pure `floor(available_gb / per_gb)`, mirroring
/// [`crate::disk_headroom::disk_headroom`]. A `per_gb` of 0 is treated as 1
/// to avoid a divide-by-zero.
#[must_use]
pub fn ram_headroom(available_gb: u64, per_gb: u64) -> usize {
    let per = per_gb.max(1);
    usize::try_from(available_gb / per).unwrap_or(usize::MAX)
}

/// Resolve the RAM-headroom concurrency bound for the current host: the
/// number of worktrees the currently-available memory can hold at the
/// resolved per-worktree estimate.
///
/// Unknown != zero (mirrors [`crate::disk_headroom::disk_headroom_limit`]):
/// when the probe is unmeasurable, this SKIPS the RAM clamp entirely —
/// returning `usize::MAX` so the RAM term never binds the `min(...)`
/// concurrency expression — and logs a warning, rather than silently treating
/// the unmeasurable probe as "no RAM available".
#[must_use]
pub fn ram_headroom_limit() -> usize {
    match available_ram_gb() {
        Some(gb) => ram_headroom(gb, per_worktree_ram_gb()),
        None => {
            log::warn!(
                "ram_headroom: could not measure available RAM on this host — skipping the RAM \
                 clamp (treating as unbounded)"
            );
            usize::MAX
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ===================================================================
    // parse_meminfo_available_kb — /proc/meminfo parsing (Linux)
    // ===================================================================

    #[test]
    fn test_parse_meminfo_reads_mem_available() {
        let out = "MemTotal:       16384000 kB\n\
                    MemFree:         2048000 kB\n\
                    MemAvailable:    8192000 kB\n\
                    Buffers:          512000 kB\n";
        assert_eq!(parse_meminfo_available_kb(out), Some(8_192_000));
    }

    #[test]
    fn test_parse_meminfo_missing_field_is_none() {
        assert_eq!(parse_meminfo_available_kb("MemTotal: 16384000 kB\n"), None);
        assert_eq!(parse_meminfo_available_kb(""), None);
    }

    #[test]
    fn test_parse_meminfo_malformed_value_is_none() {
        assert_eq!(parse_meminfo_available_kb("MemAvailable: not-a-number kB\n"), None);
    }

    // ===================================================================
    // parse_vm_stat_available_pages — vm_stat parsing (macOS)
    // ===================================================================

    #[test]
    fn test_parse_vm_stat_sums_free_and_inactive() {
        let out = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                    Pages free:                              10000.\n\
                    Pages active:                            50000.\n\
                    Pages inactive:                           5000.\n\
                    Pages speculative:                         100.\n";
        assert_eq!(parse_vm_stat_available_pages(out), Some(15_000));
    }

    #[test]
    fn test_parse_vm_stat_missing_field_is_none() {
        assert_eq!(
            parse_vm_stat_available_pages("Pages free: 10000.\n"),
            None,
            "missing 'Pages inactive'"
        );
        assert_eq!(parse_vm_stat_available_pages(""), None);
    }

    // ===================================================================
    // ram_headroom — pure floor division
    // ===================================================================

    #[test]
    fn test_ram_headroom_floors() {
        assert_eq!(ram_headroom(20, 2), 10);
        assert_eq!(ram_headroom(21, 2), 10); // floor
        assert_eq!(ram_headroom(1, 2), 0); // less than one worktree fits
        assert_eq!(ram_headroom(0, 2), 0);
    }

    #[test]
    fn test_ram_headroom_per_gb_zero_treated_as_one() {
        assert_eq!(ram_headroom(5, 0), 5);
    }

    // ===================================================================
    // per_worktree_ram_gb — env resolution
    // ===================================================================

    #[test]
    #[serial]
    fn test_per_worktree_ram_gb_default_and_override() {
        std::env::remove_var(PER_WORKTREE_RAM_GB_ENV);
        assert_eq!(per_worktree_ram_gb(), DEFAULT_PER_WORKTREE_RAM_GB);

        std::env::set_var(PER_WORKTREE_RAM_GB_ENV, "4");
        assert_eq!(per_worktree_ram_gb(), 4);

        // Zero and unparseable fall back to the default.
        std::env::set_var(PER_WORKTREE_RAM_GB_ENV, "0");
        assert_eq!(per_worktree_ram_gb(), DEFAULT_PER_WORKTREE_RAM_GB);
        std::env::set_var(PER_WORKTREE_RAM_GB_ENV, "garbage");
        assert_eq!(per_worktree_ram_gb(), DEFAULT_PER_WORKTREE_RAM_GB);
        std::env::remove_var(PER_WORKTREE_RAM_GB_ENV);
    }

    // ===================================================================
    // available_ram_gb / ram_headroom_limit — smoke tests against the real host
    // ===================================================================

    #[test]
    fn test_available_ram_gb_returns_a_plausible_value_or_none() {
        // Integration smoke: on Linux/macOS this should return Some(<plausible
        // value>); on any other platform it is unconditionally None. Either
        // way it must never panic, and a Some value must be sane.
        if let Some(gb) = available_ram_gb() {
            assert!(gb < 100_000, "no real host has 100+ TB of available RAM");
        }
    }

    #[test]
    #[serial]
    fn test_ram_headroom_limit_never_panics() {
        // Whatever this host reports, the limit must be well-typed — either a
        // real headroom count or the unmeasurable sentinel.
        let limit = ram_headroom_limit();
        assert!(limit == usize::MAX || limit < 100_000_000);
    }
}
