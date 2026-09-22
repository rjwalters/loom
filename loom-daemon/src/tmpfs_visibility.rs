//! tmpfs/`shared`-RAM and kernel OOM-kill visibility (issue #8572, split from
//! #8512).
//!
//! # Why this exists
//!
//! #8512's incident — a sweep's `/dev/shm/cargo-target-8259` pinned 6.2 GB of
//! RAM for 2.5 days and drove a 15.7 GiB / 8 vCPU fleet worker into a kernel
//! OOM-kill storm — was invisible to every signal the fleet already had.
//! [`crate::tmpfs_reclaim`] shipped the **reclaim** half; this module is the
//! **visibility** half: it turns `/proc/meminfo`'s `Shmem` figure and
//! `/proc/vmstat`'s cumulative `oom_kill` counter, plus a per-mount tmpfs
//! breakdown, into a snapshot two independent consumers render —
//! `loom-daemon health` ([`crate::health::tmpfs_visibility_section`]) and the
//! work finder's bounded warning ([`crate::work_finder::tmpfs_warning`]).
//!
//! # Reuse, not re-parsing
//!
//! Per-mount classification reuses
//! [`crate::tmpfs_reclaim::ram_backed_mount_points`] (and its pure fixture-
//! testable core, `ram_backed_mount_points_from`) rather than re-parsing
//! `/proc/mounts` a second time — the same mount table, the same `tmpfs`/
//! `ramfs` filter, one source of truth.
//!
//! # Degrade silently, never fabricate
//!
//! Every field here is `Option`: a missing/unreadable `/proc/meminfo` or
//! `/proc/vmstat` (macOS has neither) yields `None` for that field, never a
//! fabricated `0` that would look identical to "measured and empty" — the
//! same "unknown != zero" contract [`crate::disk_headroom`] and
//! [`crate::ram_headroom`] already follow. [`TmpfsVisibilitySnapshot::is_empty`]
//! is what [`crate::health::tmpfs_visibility_section`] uses to omit the
//! section entirely on a host with nothing measurable at all, rather than
//! rendering an all-`null` line.
//!
//! # Why shell to `df` for per-mount usage
//!
//! Mirrors [`crate::disk_headroom`]'s own rationale: no `statvfs`/`nix`/
//! `sysinfo` crate dependency, `df -Pk <mount>` is the same tool already used
//! for the disk-headroom probe, and the parsing stays a pure, fixture-testable
//! function ([`parse_df_used_bytes`]) split from the I/O
//! ([`mount_used_bytes`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::tmpfs_reclaim::{human_size, ram_backed_mount_points, MountEntry};

// ============================================================================
// File-source overrides (fixture-testable, mirrors tmpfs_reclaim::MOUNTS_FILE_ENV)
// ============================================================================

/// Override for which file is read as `/proc/meminfo`. Production always uses
/// the real path; tests point this at a fixture so meminfo parsing is
/// exercised without depending on the real host.
pub const MEMINFO_FILE_ENV: &str = "LOOM_TMPFS_VISIBILITY_MEMINFO_FILE";
const DEFAULT_MEMINFO_FILE: &str = "/proc/meminfo";

/// Override for which file is read as `/proc/vmstat`.
pub const VMSTAT_FILE_ENV: &str = "LOOM_TMPFS_VISIBILITY_VMSTAT_FILE";
const DEFAULT_VMSTAT_FILE: &str = "/proc/vmstat";

/// A RAM-backed mount is only worth naming individually once it holds at
/// least this much — a near-empty `tmpfs` (e.g. a bare `/dev/shm` with a few
/// KB of socket files) is noise, not a finding.
pub const DEFAULT_MOUNT_FLOOR_BYTES: u64 = 64 * 1024 * 1024;

/// Default work-finder warning threshold: `shmem / total` >= 15% of host RAM.
pub const DEFAULT_WARN_FRACTION_PERCENT: f64 = 15.0;

// ============================================================================
// /proc/meminfo parsing (pure)
// ============================================================================

/// The subset of `/proc/meminfo` this module reads, each `None` when the
/// field is absent/malformed — never a fabricated `0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeminfoSnapshot {
    pub total_kb: Option<u64>,
    pub available_kb: Option<u64>,
    /// `Shmem` — tmpfs-resident + SysV/POSIX shared-memory pages, the same
    /// figure `free -h`'s `shared` column reports.
    pub shmem_kb: Option<u64>,
}

fn meminfo_field(contents: &str, key: &str) -> Option<u64> {
    let prefix = format!("{key}:");
    let line = contents.lines().find(|l| l.starts_with(&prefix))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Parse `/proc/meminfo` contents into [`MeminfoSnapshot`]. Pure — every field
/// independently defaults to `None` on a missing/malformed line, so a kernel
/// that omits one field does not blind the others.
#[must_use]
pub fn parse_meminfo(contents: &str) -> MeminfoSnapshot {
    MeminfoSnapshot {
        total_kb: meminfo_field(contents, "MemTotal"),
        available_kb: meminfo_field(contents, "MemAvailable"),
        shmem_kb: meminfo_field(contents, "Shmem"),
    }
}

fn read_meminfo() -> MeminfoSnapshot {
    let path = std::env::var(MEMINFO_FILE_ENV).unwrap_or_else(|_| DEFAULT_MEMINFO_FILE.to_string());
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_meminfo(&contents),
        Err(_) => MeminfoSnapshot::default(),
    }
}

// ============================================================================
// /proc/vmstat oom_kill parsing (pure)
// ============================================================================

/// Parse the cumulative kernel OOM-kill count from `/proc/vmstat` contents
/// (the `oom_kill` line — present since Linux 4.20-ish under
/// `CONFIG_VM_EVENT_COUNTERS`, which every distro kernel Loom targets
/// enables). `None` when the line is absent (older kernel, or a fixture that
/// never wrote it) or malformed — never a fabricated `0`, which would read
/// identically to "measured, zero kills since boot".
#[must_use]
pub fn parse_vmstat_oom_kill(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|l| l.starts_with("oom_kill "))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn read_oom_kill_count() -> Option<u64> {
    let path = std::env::var(VMSTAT_FILE_ENV).unwrap_or_else(|_| DEFAULT_VMSTAT_FILE.to_string());
    let contents = std::fs::read_to_string(path).ok()?;
    parse_vmstat_oom_kill(&contents)
}

// ============================================================================
// Per-mount tmpfs usage (`df -Pk`, mirrors crate::disk_headroom)
// ============================================================================

/// One RAM-backed mount's usage, once it clears [`DEFAULT_MOUNT_FLOOR_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpfsMountUsage {
    pub mount_point: PathBuf,
    pub used_bytes: u64,
}

/// Parse the "Used" column (1024-blocks, 0-based column index 2) from `df
/// -Pk`'s single data row, in bytes. `None` on malformed output (missing data
/// row, non-numeric column) — mirrors
/// [`crate::disk_headroom::parse_df_available_gb`]'s contract exactly.
#[must_use]
pub fn parse_df_used_bytes(df_output: &str) -> Option<u64> {
    let data_row = df_output.lines().nth(1)?;
    let used_k: u64 = data_row.split_whitespace().nth(2)?.parse().ok()?;
    Some(used_k.saturating_mul(1024))
}

fn mount_used_bytes(mount_point: &Path) -> Option<u64> {
    let output = Command::new("df")
        .arg("-Pk")
        .arg(mount_point)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df_used_bytes(&String::from_utf8_lossy(&output.stdout))
}

/// Every RAM-backed mount's usage, filtered to those at/above `floor_bytes`
/// and sorted by descending usage (the biggest offender first, matching what
/// both consumers want to name).
#[must_use]
pub fn mount_usages(mounts: &[MountEntry], floor_bytes: u64) -> Vec<TmpfsMountUsage> {
    let mut usages: Vec<TmpfsMountUsage> = mounts
        .iter()
        .filter_map(|m| {
            let used_bytes = mount_used_bytes(&m.mount_point)?;
            (used_bytes >= floor_bytes).then_some(TmpfsMountUsage {
                mount_point: m.mount_point.clone(),
                used_bytes,
            })
        })
        .collect();
    usages.sort_by(|a, b| b.used_bytes.cmp(&a.used_bytes));
    usages
}

/// The largest reported mount, if any — the "biggest contributor" both the
/// health line and the work-finder warning name.
#[must_use]
pub fn largest_mount(mounts: &[TmpfsMountUsage]) -> Option<&TmpfsMountUsage> {
    mounts.first()
}

// ============================================================================
// The collected snapshot
// ============================================================================

/// Everything this module knows about host memory/tmpfs at one point in time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TmpfsVisibilitySnapshot {
    pub total_kb: Option<u64>,
    pub available_kb: Option<u64>,
    pub shmem_kb: Option<u64>,
    pub oom_kill_count: Option<u64>,
    /// Per-mount breakdown, biggest first, filtered to
    /// [`DEFAULT_MOUNT_FLOOR_BYTES`].
    pub mounts: Vec<TmpfsMountUsage>,
}

impl TmpfsVisibilitySnapshot {
    /// Nothing measurable at all — the macOS / no-`/proc` case. Consumers use
    /// this to degrade silently (omit the section / never warn) instead of
    /// rendering an all-`null` line.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_kb.is_none()
            && self.available_kb.is_none()
            && self.shmem_kb.is_none()
            && self.oom_kill_count.is_none()
            && self.mounts.is_empty()
    }

    /// `shmem / total`, as a fraction in `0.0..=1.0`. `None` when either
    /// input is unmeasurable or `total_kb` is `0` (never a divide-by-zero, and
    /// never a fabricated fraction from a missing numerator/denominator).
    #[must_use]
    pub fn shmem_fraction(&self) -> Option<f64> {
        shmem_fraction_of_total(self.shmem_kb, self.total_kb)
    }

    /// Render the human-readable memory-detail line for `loom-daemon health`.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut parts = Vec::new();
        if let (Some(shmem_kb), Some(total_kb)) = (self.shmem_kb, self.total_kb) {
            let pct = self.shmem_fraction().map_or(0.0, |f| f * 100.0);
            parts.push(format!(
                "shared {} of {} total ({pct:.1}%)",
                human_size(shmem_kb * 1024),
                human_size(total_kb * 1024)
            ));
        } else if let Some(shmem_kb) = self.shmem_kb {
            parts.push(format!("shared {}", human_size(shmem_kb * 1024)));
        }
        if let Some(count) = self.oom_kill_count {
            parts.push(format!("oom_kill={count}"));
        }
        if let Some(biggest) = largest_mount(&self.mounts) {
            parts.push(format!(
                "largest tmpfs mount {} ({})",
                biggest.mount_point.display(),
                human_size(biggest.used_bytes)
            ));
        }
        if parts.is_empty() {
            "no tmpfs/shared-RAM data available on this host".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// `shmem / total`, pure. `None` on a missing input or a zero denominator.
#[must_use]
pub fn shmem_fraction_of_total(shmem_kb: Option<u64>, total_kb: Option<u64>) -> Option<f64> {
    let shmem_kb = shmem_kb?;
    let total_kb = total_kb?;
    if total_kb == 0 {
        return None;
    }
    Some(shmem_kb as f64 / total_kb as f64)
}

/// Collect the full snapshot: `/proc/meminfo`, `/proc/vmstat`, and — reusing
/// [`crate::tmpfs_reclaim::ram_backed_mount_points`] rather than re-parsing
/// `/proc/mounts` — a `df`-derived per-mount breakdown. Every step degrades
/// independently and silently; there is no failure mode that panics or
/// fabricates a reading.
#[must_use]
pub fn collect() -> TmpfsVisibilitySnapshot {
    let meminfo = read_meminfo();
    let oom_kill_count = read_oom_kill_count();
    let mounts = mount_usages(&ram_backed_mount_points(), DEFAULT_MOUNT_FLOOR_BYTES);
    TmpfsVisibilitySnapshot {
        total_kb: meminfo.total_kb,
        available_kb: meminfo.available_kb,
        shmem_kb: meminfo.shmem_kb,
        oom_kill_count,
        mounts,
    }
}

// ============================================================================
// Config (.loom/config.json → autonomous.tmpfsVisibility) — env > config > default
// ============================================================================

/// Master on/off env override for the **work-finder warning only** — the
/// `loom-daemon health` memory-detail line is unconditional (an informational
/// reading, like `ram`/`disk` headroom) and never gated by this. Default-on:
/// `0`/`false`/`no`/`off` disables, `1`/`true`/`yes`/`on` force-enables.
pub const WARN_ENABLE_ENV: &str = "LOOM_TMPFS_VISIBILITY_WARN";

/// Env override for the warn-fraction threshold (percent, e.g. `15` for 15%).
pub const WARN_FRACTION_PERCENT_ENV: &str = "LOOM_TMPFS_VISIBILITY_WARN_FRACTION_PERCENT";

/// The subset of `.loom/config.json → autonomous.tmpfsVisibility` this module
/// consumes. Every field `Option`, matching every other `autonomous.*`
/// surface's env > config > default precedence.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TmpfsVisibilityConfig {
    /// `…tmpfsVisibility.warnEnabled` (default **true**) — gates the
    /// work-finder warning only.
    pub warn_enabled: Option<bool>,
    /// `…tmpfsVisibility.warnFractionPercent` (default
    /// [`DEFAULT_WARN_FRACTION_PERCENT`]).
    pub warn_fraction_percent: Option<f64>,
}

/// Read `.loom/config.json → autonomous.tmpfsVisibility`, soft-failing every
/// field to `None` (env/default resolution) on a missing file, malformed
/// JSON, or a missing block.
#[must_use]
pub fn read_tmpfs_visibility_config(repo_root: &Path) -> TmpfsVisibilityConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.tmpfsVisibility")
    else {
        return TmpfsVisibilityConfig::default();
    };
    TmpfsVisibilityConfig {
        warn_enabled: block
            .get("warnEnabled")
            .and_then(serde_json::Value::as_bool),
        warn_fraction_percent: block
            .get("warnFractionPercent")
            .and_then(serde_json::Value::as_f64)
            .filter(|&f| f > 0.0),
    }
}

/// Resolve whether the work-finder warning is armed — precedence **env >
/// config > default(true)**.
#[must_use]
pub fn resolve_warn_enabled(config: &TmpfsVisibilityConfig) -> bool {
    if let Ok(v) = std::env::var(WARN_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.warn_enabled.unwrap_or(true)
}

/// Resolve the warn-fraction threshold (percent) — precedence **env > config
/// > default**.
#[must_use]
pub fn resolve_warn_fraction_percent(config: &TmpfsVisibilityConfig) -> f64 {
    std::env::var(WARN_FRACTION_PERCENT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|&f| f > 0.0)
        .or(config.warn_fraction_percent)
        .unwrap_or(DEFAULT_WARN_FRACTION_PERCENT)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ===================================================================
    // parse_meminfo — /proc/meminfo parsing (pure)
    // ===================================================================

    #[test]
    fn parse_meminfo_reads_all_three_fields() {
        let out = "MemTotal:       16384000 kB\n\
                    MemFree:         2048000 kB\n\
                    MemAvailable:    8192000 kB\n\
                    Shmem:            512000 kB\n";
        let snap = parse_meminfo(out);
        assert_eq!(snap.total_kb, Some(16_384_000));
        assert_eq!(snap.available_kb, Some(8_192_000));
        assert_eq!(snap.shmem_kb, Some(512_000));
    }

    #[test]
    fn parse_meminfo_missing_fields_are_independently_none() {
        let snap = parse_meminfo("MemTotal: 16384000 kB\n");
        assert_eq!(snap.total_kb, Some(16_384_000));
        assert_eq!(snap.available_kb, None);
        assert_eq!(snap.shmem_kb, None);
    }

    #[test]
    fn parse_meminfo_empty_is_all_none() {
        let snap = parse_meminfo("");
        assert_eq!(snap, MeminfoSnapshot::default());
    }

    // ===================================================================
    // parse_vmstat_oom_kill — /proc/vmstat parsing (pure)
    // ===================================================================

    #[test]
    fn parse_vmstat_oom_kill_reads_the_field() {
        let out = "nr_free_pages 12345\noom_kill 3\npgfault 999\n";
        assert_eq!(parse_vmstat_oom_kill(out), Some(3));
    }

    #[test]
    fn parse_vmstat_oom_kill_missing_is_none() {
        assert_eq!(parse_vmstat_oom_kill("nr_free_pages 12345\n"), None);
        assert_eq!(parse_vmstat_oom_kill(""), None);
    }

    #[test]
    fn parse_vmstat_oom_kill_malformed_is_none() {
        assert_eq!(parse_vmstat_oom_kill("oom_kill not-a-number\n"), None);
    }

    // ===================================================================
    // parse_df_used_bytes — df -Pk parsing (pure, mirrors disk_headroom)
    // ===================================================================

    #[test]
    fn parse_df_used_bytes_linux_shape() {
        let out = "Filesystem     1024-blocks    Used Available Use% Mounted on\n\
                    tmpfs             8192000  512000   7680000    7% /dev/shm\n";
        assert_eq!(parse_df_used_bytes(out), Some(512_000 * 1024));
    }

    #[test]
    fn parse_df_used_bytes_missing_data_row_is_none() {
        assert_eq!(parse_df_used_bytes("only a header\n"), None);
        assert_eq!(parse_df_used_bytes(""), None);
    }

    #[test]
    fn parse_df_used_bytes_non_numeric_is_none() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted\n\
                    tmpfs 999 not-a-number 1 1% /dev/shm\n";
        assert_eq!(parse_df_used_bytes(out), None);
    }

    // ===================================================================
    // shmem_fraction_of_total — pure math
    // ===================================================================

    #[test]
    fn shmem_fraction_computes_the_ratio() {
        assert_eq!(shmem_fraction_of_total(Some(2_000), Some(10_000)), Some(0.2));
    }

    #[test]
    fn shmem_fraction_missing_input_is_none() {
        assert_eq!(shmem_fraction_of_total(None, Some(10_000)), None);
        assert_eq!(shmem_fraction_of_total(Some(2_000), None), None);
    }

    #[test]
    fn shmem_fraction_zero_total_is_none_not_a_panic() {
        assert_eq!(shmem_fraction_of_total(Some(2_000), Some(0)), None);
    }

    // ===================================================================
    // mount_usages / largest_mount — pure filtering + sort
    // ===================================================================

    fn usage(path: &str, bytes: u64) -> TmpfsMountUsage {
        TmpfsMountUsage {
            mount_point: PathBuf::from(path),
            used_bytes: bytes,
        }
    }

    #[test]
    fn largest_mount_picks_the_biggest() {
        let mounts = vec![
            usage("/dev/shm", 100),
            usage("/tmp", 900),
            usage("/run", 50),
        ];
        assert_eq!(largest_mount(&mounts).unwrap().mount_point, PathBuf::from("/tmp"));
    }

    #[test]
    fn largest_mount_empty_is_none() {
        assert_eq!(largest_mount(&[]), None);
    }

    // ===================================================================
    // TmpfsVisibilitySnapshot::is_empty / shmem_fraction / render_summary
    // ===================================================================

    #[test]
    fn empty_snapshot_is_empty() {
        assert!(TmpfsVisibilitySnapshot::default().is_empty());
    }

    #[test]
    fn a_snapshot_with_any_field_is_not_empty() {
        let snap = TmpfsVisibilitySnapshot {
            oom_kill_count: Some(0),
            ..Default::default()
        };
        assert!(!snap.is_empty());
    }

    #[test]
    fn render_summary_reports_shared_total_and_oom_kill() {
        let snap = TmpfsVisibilitySnapshot {
            total_kb: Some(16_000_000),
            shmem_kb: Some(2_000_000),
            oom_kill_count: Some(0),
            ..Default::default()
        };
        let summary = snap.render_summary();
        assert!(summary.contains("shared"), "{summary}");
        assert!(summary.contains("12.5%"), "{summary}");
        assert!(summary.contains("oom_kill=0"), "{summary}");
    }

    #[test]
    fn render_summary_names_the_largest_mount() {
        let snap = TmpfsVisibilitySnapshot {
            mounts: vec![usage("/dev/shm", 900 * 1024 * 1024)],
            ..Default::default()
        };
        let summary = snap.render_summary();
        assert!(summary.contains("/dev/shm"), "{summary}");
    }

    #[test]
    fn render_summary_on_an_empty_snapshot_says_so_rather_than_fabricating() {
        let summary = TmpfsVisibilitySnapshot::default().render_summary();
        assert!(summary.contains("no tmpfs/shared-RAM data"), "{summary}");
    }

    // ===================================================================
    // collect() — I/O smoke test against injected fixture files (#8572: must
    // pass on macOS, so this drives fixtures, never the live host)
    // ===================================================================

    #[test]
    #[serial]
    fn collect_reads_injected_meminfo_and_vmstat_fixtures() {
        let meminfo = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(meminfo.path(), "MemTotal: 1000 kB\nMemAvailable: 500 kB\nShmem: 100 kB\n")
            .unwrap();
        let vmstat = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(vmstat.path(), "oom_kill 7\n").unwrap();

        std::env::set_var(MEMINFO_FILE_ENV, meminfo.path());
        std::env::set_var(VMSTAT_FILE_ENV, vmstat.path());
        // No injected mounts file for tmpfs_reclaim's own env var — a missing
        // /proc/mounts degrades to an empty mount list, exercised on its own
        // in tmpfs_reclaim's test suite.
        std::env::set_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV, "/nonexistent-for-test");

        let snap = collect();

        std::env::remove_var(MEMINFO_FILE_ENV);
        std::env::remove_var(VMSTAT_FILE_ENV);
        std::env::remove_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV);

        assert_eq!(snap.total_kb, Some(1000));
        assert_eq!(snap.available_kb, Some(500));
        assert_eq!(snap.shmem_kb, Some(100));
        assert_eq!(snap.oom_kill_count, Some(7));
        assert!(snap.mounts.is_empty());
        assert!(!snap.is_empty());
    }

    #[test]
    #[serial]
    fn collect_degrades_silently_on_missing_files_macos_shape() {
        std::env::set_var(MEMINFO_FILE_ENV, "/nonexistent-meminfo-for-test");
        std::env::set_var(VMSTAT_FILE_ENV, "/nonexistent-vmstat-for-test");
        std::env::set_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV, "/nonexistent-mounts-for-test");

        let snap = collect();

        std::env::remove_var(MEMINFO_FILE_ENV);
        std::env::remove_var(VMSTAT_FILE_ENV);
        std::env::remove_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV);

        assert!(
            snap.is_empty(),
            "a host with no readable /proc/meminfo, /proc/vmstat or /proc/mounts \
             (e.g. macOS) must degrade to an empty snapshot, never fabricate zeros: {snap:?}"
        );
    }

    // ===================================================================
    // Config resolution — env > config > default
    // ===================================================================

    #[test]
    fn resolve_warn_enabled_defaults_true() {
        assert!(resolve_warn_enabled(&TmpfsVisibilityConfig::default()));
    }

    #[test]
    fn resolve_warn_enabled_config_overrides_default() {
        let config = TmpfsVisibilityConfig {
            warn_enabled: Some(false),
            ..Default::default()
        };
        assert!(!resolve_warn_enabled(&config));
    }

    #[test]
    #[serial]
    fn resolve_warn_enabled_env_overrides_config() {
        let config = TmpfsVisibilityConfig {
            warn_enabled: Some(false),
            ..Default::default()
        };
        std::env::set_var(WARN_ENABLE_ENV, "1");
        assert!(resolve_warn_enabled(&config));
        std::env::remove_var(WARN_ENABLE_ENV);
    }

    #[test]
    fn resolve_warn_fraction_percent_defaults() {
        assert_eq!(
            resolve_warn_fraction_percent(&TmpfsVisibilityConfig::default()),
            DEFAULT_WARN_FRACTION_PERCENT
        );
    }

    #[test]
    fn resolve_warn_fraction_percent_config_overrides_default() {
        let config = TmpfsVisibilityConfig {
            warn_fraction_percent: Some(25.0),
            ..Default::default()
        };
        assert!((resolve_warn_fraction_percent(&config) - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    #[serial]
    fn resolve_warn_fraction_percent_env_overrides_config() {
        let config = TmpfsVisibilityConfig {
            warn_fraction_percent: Some(25.0),
            ..Default::default()
        };
        std::env::set_var(WARN_FRACTION_PERCENT_ENV, "40");
        assert!((resolve_warn_fraction_percent(&config) - 40.0).abs() < f64::EPSILON);
        std::env::remove_var(WARN_FRACTION_PERCENT_ENV);
    }

    #[test]
    #[serial]
    fn resolve_warn_fraction_percent_ignores_non_positive_env() {
        let config = TmpfsVisibilityConfig::default();
        std::env::set_var(WARN_FRACTION_PERCENT_ENV, "0");
        assert_eq!(resolve_warn_fraction_percent(&config), DEFAULT_WARN_FRACTION_PERCENT);
        std::env::set_var(WARN_FRACTION_PERCENT_ENV, "garbage");
        assert_eq!(resolve_warn_fraction_percent(&config), DEFAULT_WARN_FRACTION_PERCENT);
        std::env::remove_var(WARN_FRACTION_PERCENT_ENV);
    }

    #[test]
    fn read_tmpfs_visibility_config_missing_block_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let config = read_tmpfs_visibility_config(tmp.path());
        assert_eq!(config, TmpfsVisibilityConfig::default());
    }

    #[test]
    fn read_tmpfs_visibility_config_reads_the_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"tmpfsVisibility":{"warnEnabled":false,"warnFractionPercent":30}}}"#,
        )
        .unwrap();
        let config = read_tmpfs_visibility_config(tmp.path());
        assert_eq!(config.warn_enabled, Some(false));
        assert_eq!(config.warn_fraction_percent, Some(30.0));
    }
}
