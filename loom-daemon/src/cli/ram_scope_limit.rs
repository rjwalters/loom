//! `loom-daemon ram-scope-limit` — the per-agent-scope `MemoryMax` that
//! `spawn-claude.sh` puts on its systemd `--user` scope (#11094, slice 2).
//!
//! The work finder records every agent scope's `memory.peak` into a per-repo
//! rolling history ([`loom_daemon::ram_peaks`], `~/.loom/ram-peaks.json`) and
//! charges admission with it. This is the containment half: a scope gets a
//! `MemoryMax` sized from the SAME history, so an over-budget build (a 13 GB
//! `rustc`) is OOM-killed INSIDE its own cgroup — with `OOMPolicy=continue`
//! only that command fails and the agent sees the error — instead of the
//! kernel's global OOM killer picking a victim.
//!
//! `MemoryMax`, not `MemoryHigh`: `MemoryHigh` only throttles/reclaims and
//! never kills, so on a no-swap host a runaway build would stall instead of
//! failing.
//!
//! Native rather than inline in `spawn-claude.sh` because that script is a
//! `contract`-category file and new logic behind its name belongs here
//! (`.loom/docs/shell-language-policy.md`); the script keeps only the call.
//!
//! # Policy
//!
//! Conservative by construction: NO history for the repo (or a missing /
//! corrupt store) means NO limit, so a fresh repo behaves exactly as before.
//! With history the limit is `max(floor, high_water * pct / 100)`.
//!
//! | Env | Meaning | Default |
//! |---|---|---|
//! | `LOOM_SWEEP_MEMORY_MAX` | `0`/`off`/`false`/`no` disables; a positive integer is an explicit MiB limit (no history needed); unset derives from history | unset |
//! | `LOOM_SWEEP_MEMORY_MAX_PCT` | multiple of the observed peak (values under 100 are ignored) | `200` |
//! | `LOOM_SWEEP_MEMORY_MAX_MIN_MB` | floor for a derived limit, MiB | `4096` |
//! | `LOOM_RAM_PEAKS_PATH` | history file (same override as the daemon) | `~/.loom/ram-peaks.json` |
//!
//! A limit at or above host RAM is pointless and is not applied.
//!
//! # Exit-code contract
//!
//! | Exit | stdout | Meaning |
//! |---|---|---|
//! | `0` | limit in MiB | apply `-p MemoryMax=<n>M` |
//! | `1` | — | no limit applies (disabled / no history / >= host RAM) — an answer, not an error |
//! | `3` | limit in MiB | `--probe` was given and systemd rejected the property (memory controller not delegated?) — run without a limit |
//!
//! Any other exit (including an older binary that lacks this subcommand) must
//! be treated as `1`: the caller degrades to no limit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;

use loom_daemon::ram_peaks;

/// No limit applies. Not an error.
const EX_NO_LIMIT: i32 = 1;
/// The probe scope rejected `MemoryMax`.
const EX_PROBE_REJECTED: i32 = 3;

const DEFAULT_PCT: u64 = 200;
const DEFAULT_FLOOR_MB: u64 = 4096;
const MIB: u64 = 1024 * 1024;

#[derive(clap::Args)]
pub(crate) struct RamScopeLimitArgs {
    /// Workspace the agent runs in; its directory name is the history key
    /// (the same key the daemon records under). Defaults to `$WORKSPACE`,
    /// then the current directory.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// Before answering, check that systemd accepts the property by running a
    /// throwaway `systemd-run --user --scope -p MemoryMax=… -- true`. Exit 3
    /// when it does not.
    #[arg(long)]
    pub probe: bool,
}

/// The `LOOM_SWEEP_MEMORY_MAX*` settings, as raw env strings.
#[derive(Debug, Default, Clone)]
pub(crate) struct Settings {
    pub max: Option<String>,
    pub pct: Option<String>,
    pub floor_mb: Option<String>,
}

impl Settings {
    fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            max: get("LOOM_SWEEP_MEMORY_MAX"),
            pct: get("LOOM_SWEEP_MEMORY_MAX_PCT"),
            floor_mb: get("LOOM_SWEEP_MEMORY_MAX_MIN_MB"),
        }
    }
}

/// The scope limit in MiB, or `None` when no limit should be applied.
/// `peak_bytes` is the repo's observed high-water mark; `total_mb` is host
/// RAM (`0` = unknown, no ceiling check).
pub(crate) fn scope_limit_mb(s: &Settings, peak_bytes: Option<u64>, total_mb: u64) -> Option<u64> {
    let max = s.max.as_deref().map(str::trim);
    if matches!(max.map(str::to_ascii_lowercase).as_deref(), Some("0" | "off" | "false" | "no")) {
        return None;
    }
    let pct = s
        .pct
        .as_deref()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&p| p >= 100)
        .unwrap_or(DEFAULT_PCT);
    let floor = s
        .floor_mb
        .as_deref()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_FLOOR_MB);
    let limit = match max.and_then(|v| v.parse::<u64>().ok()).filter(|&v| v > 0) {
        Some(explicit) => explicit,
        None => {
            let peak = peak_bytes.filter(|&b| b > 0)?;
            (u128::from(peak) * u128::from(pct) / 100 / u128::from(MIB))
                .try_into()
                .unwrap_or(u64::MAX)
                .max(floor)
        }
    };
    (total_mb == 0 || limit < total_mb).then_some(limit)
}

/// Host RAM in MiB from `/proc/meminfo`, `0` when unreadable.
fn host_total_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| loom_daemon::host_pressure::parse_meminfo(&t).mem_total_kb)
        .map_or(0, |kb| kb / 1024)
}

/// The repo's observed high-water mark from the peaks store.
fn observed_peak(workspace: &Path) -> Option<u64> {
    let store = ram_peaks::load(&ram_peaks::store_path()?);
    let key = ram_peaks::repo_key(workspace);
    ram_peaks::high_water_bytes(store.repos.get(&key)?)
}

/// Whether systemd accepts `MemoryMax=<mb>M` on a throwaway `--user` scope.
/// The unit name carries `spawn-claude.sh`'s probe prefix so the daemon's
/// scope discovery skips it.
fn systemd_accepts(mb: u64) -> bool {
    let unit = format!(
        "loom-agent-probe-{}-{}.scope",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    );
    Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", &format!("--unit={unit}")])
        .args(["-p", &format!("MemoryMax={mb}M"), "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl RamScopeLimitArgs {
    pub(crate) fn run(self) -> Result<()> {
        let workspace = self
            .workspace
            .or_else(|| {
                std::env::var_os("WORKSPACE")
                    .filter(|w| !w.is_empty())
                    .map(PathBuf::from)
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let settings = Settings::from_env();
        let peak = observed_peak(&workspace);
        let Some(mb) = scope_limit_mb(&settings, peak, host_total_mb()) else {
            std::process::exit(EX_NO_LIMIT);
        };
        println!("{mb}");
        if self.probe && !systemd_accepts(mb) {
            std::process::exit(EX_PROBE_REJECTED);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn s(max: Option<&str>, pct: Option<&str>, floor: Option<&str>) -> Settings {
        Settings {
            max: max.map(String::from),
            pct: pct.map(String::from),
            floor_mb: floor.map(String::from),
        }
    }

    #[test]
    fn no_history_means_no_limit() {
        assert_eq!(scope_limit_mb(&Settings::default(), None, 30_000), None);
        assert_eq!(scope_limit_mb(&Settings::default(), Some(0), 30_000), None);
    }

    #[test]
    fn derived_limit_is_pct_of_high_water() {
        assert_eq!(scope_limit_mb(&Settings::default(), Some(13 * GIB), 65_536), Some(26_624));
        assert_eq!(
            scope_limit_mb(&s(None, Some("150"), None), Some(13 * GIB), 65_536),
            Some(19_968)
        );
    }

    #[test]
    fn pct_under_100_or_garbage_falls_back_to_default() {
        assert_eq!(
            scope_limit_mb(&s(None, Some("50"), None), Some(13 * GIB), 65_536),
            Some(26_624)
        );
        assert_eq!(scope_limit_mb(&s(None, Some("x"), None), Some(13 * GIB), 65_536), Some(26_624));
    }

    #[test]
    fn small_peak_is_raised_to_the_floor() {
        assert_eq!(scope_limit_mb(&Settings::default(), Some(GIB), 30_000), Some(4096));
        assert_eq!(scope_limit_mb(&s(None, None, Some("8000")), Some(GIB), 30_000), Some(8000));
    }

    #[test]
    fn limit_at_or_above_host_ram_is_not_applied() {
        assert_eq!(scope_limit_mb(&Settings::default(), Some(13 * GIB), 20_000), None);
        assert_eq!(scope_limit_mb(&Settings::default(), Some(13 * GIB), 26_624), None);
        // Unknown host RAM: no ceiling check.
        assert_eq!(scope_limit_mb(&Settings::default(), Some(13 * GIB), 0), Some(26_624));
    }

    #[test]
    fn disable_values() {
        for v in ["0", "off", "OFF", "false", "no", "No"] {
            assert_eq!(
                scope_limit_mb(&s(Some(v), None, None), Some(13 * GIB), 65_536),
                None,
                "{v}"
            );
        }
    }

    #[test]
    fn explicit_limit_needs_no_history() {
        assert_eq!(scope_limit_mb(&s(Some("8000"), None, None), None, 30_000), Some(8000));
        assert_eq!(scope_limit_mb(&s(Some("8000"), None, None), None, 6000), None);
    }

    #[test]
    fn non_numeric_setting_falls_back_to_history() {
        assert_eq!(
            scope_limit_mb(&s(Some("auto"), None, None), Some(13 * GIB), 65_536),
            Some(26_624)
        );
        assert_eq!(scope_limit_mb(&s(Some("auto"), None, None), None, 65_536), None);
    }
}
